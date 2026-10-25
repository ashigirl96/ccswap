//! Blocking work runs on worker threads whose results a 1-second main-thread
//! tick drains, keeping AppKit responsive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ccswap_claude::actions::Actions;
use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::autoswitch::{
    AutoSwitchEngine, AutoSwitchEvent, AutoSwitchSettings, EventKind, LoopHandle, SwitcherHost,
};
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_claude::snapshot::SnapshotSource;
use ccswap_claude::switcher::SwitchStrategy;
use ccswap_codex::{
    AddOutcome, CodexPaths, CodexStore, CodexUsage, LoginOutcome, LoginStdio, SwitchOutcome,
};
use ccswap_core::console::Console;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::Environment;
use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::fsutil;
use ccswap_core::log;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{Style, Theme};
use ccswap_core::settings::Settings;
use ccswap_core::usage::now_secs;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer};

use super::appkit::{self, NOTIFICATION_TITLE};
use super::format::{
    MenuAccount, MenuSnapshot, SWITCH_HISTORY_LIMIT, codex_accounts, format_title,
    format_usage_log, parse_switch_history, usage_log_key,
};
use super::menu::{Command, MenuInput, build_menu};
use super::settings::{FILENAME, MenuBarSettings};
use super::widget;

const SWITCHED_MESSAGE: &str =
    "Switch takes effect within ~30s — restart Claude Code to apply immediately.";
const KEYCHAIN_BLOCKED: &str = "Couldn't read the active credential. If the menu bar is running \
    as a background/login agent, macOS blocks its Keychain access — quit and relaunch it from \
    a Terminal with: ccswap menubar";

enum Report {
    Snapshot(MenuSnapshot),
    Action {
        /// `Err` = alert text; `Ok(Some)` = a notification to post.
        outcome: std::result::Result<Option<Notice>, String>,
    },
}

struct Notice {
    subtitle: &'static str,
    message: String,
}

impl Notice {
    fn switched() -> Self {
        Notice {
            subtitle: "Account switched",
            message: SWITCHED_MESSAGE.into(),
        }
    }
}

type UsageLogKeys = HashMap<String, (Option<f64>, Option<f64>)>;

pub struct Controller {
    actions: Arc<dyn Actions>,
    env: Environment,
    paths: Paths,
    settings_path: PathBuf,
    log_path: PathBuf,
    widget_feed_path: PathBuf,
    widget_request_path: PathBuf,
    settings: RefCell<MenuBarSettings>,
    snapshot: RefCell<MenuSnapshot>,
    source: Arc<SnapshotSource>,
    refreshing: Arc<AtomicBool>,
    /// Two `codex login`s would fight over Codex's fixed callback port.
    codex_login_running: Arc<AtomicBool>,
    usage_logged: Arc<Mutex<UsageLogKeys>>,
    reports: (Sender<Report>, Receiver<Report>),
    events: (Sender<AutoSwitchEvent>, Receiver<AutoSwitchEvent>),
    engine: RefCell<Option<LoopHandle>>,
    config_mtime: Cell<Option<SystemTime>>,
    openai_enabled: Cell<bool>,
    commands: RefCell<Vec<Command>>,
    status_item: Retained<NSStatusItem>,
    refresh_timer: RefCell<Option<Retained<NSTimer>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CcswapMenuBarTarget"]
    #[ivars = Controller]
    pub struct Target;

    impl Target {
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            let command = self.ivars().commands.borrow().get(sender.tag() as usize).cloned();
            if let Some(command) = command {
                self.run_command(command);
            }
        }

        #[unsafe(method(syncTick:))]
        fn sync_tick(&self, _timer: &NSTimer) {
            self.sync();
        }

        #[unsafe(method(refreshTick:))]
        fn refresh_tick(&self, _timer: &NSTimer) {
            self.refresh_async();
        }
    }
);

pub fn run(actions: Arc<dyn Actions>, env: Environment) -> i32 {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("Error: the menu bar must run on the main thread");
        return 1;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);

    let paths = Paths::new(&env);
    let proxy = ccswap::openai::proxy_token(&paths).and_then(|_| {
        std::net::TcpListener::bind(ccswap::openai::PROXY_ADDRESS)
            .map_err(|e| Error::config(format!("Could not start the OpenAI proxy: {e}")))
    });
    match proxy {
        Ok(listener) => {
            let proxy_paths = paths.clone();
            let codex_paths = CodexPaths::new(&env);
            let endpoints = Endpoints::new(&env);
            std::thread::spawn(move || {
                if let Err(error) =
                    ccswap::openai::serve(listener, proxy_paths, codex_paths, endpoints)
                {
                    log::warning(format!("OpenAI proxy stopped: {error}"));
                }
            });
        }
        Err(error) => appkit::alert(mtm, "OpenAI proxy", &error.to_string()),
    }
    let backup = actions.backup_dir();
    let settings_path = backup.join(FILENAME);
    let controller = Controller {
        settings: RefCell::new(MenuBarSettings::load(&settings_path)),
        settings_path,
        log_path: backup.join("claude-swap.log"),
        widget_feed_path: backup.join(widget::DIRNAME).join(widget::FEED_FILENAME),
        widget_request_path: backup.join(widget::DIRNAME).join(widget::REQUEST_FILENAME),
        source: Arc::new(SnapshotSource::new(actions.clone())),
        actions,
        env,
        paths,
        snapshot: RefCell::new(MenuSnapshot::default()),
        refreshing: Arc::new(AtomicBool::new(false)),
        codex_login_running: Arc::new(AtomicBool::new(false)),
        usage_logged: Arc::default(),
        reports: channel(),
        events: channel(),
        engine: RefCell::new(None),
        config_mtime: Cell::new(None),
        openai_enabled: Cell::new(false),
        commands: RefCell::new(Vec::new()),
        status_item,
        refresh_timer: RefCell::new(None),
    };
    let target = Target::new(mtm, controller);
    target.rebuild_menu();
    target.schedule_refresh_timer();
    target.add_timer(1.0, sel!(syncTick:));
    target.refresh_async();
    if target.ivars().settings.borrow().auto_switch_enabled {
        target.start_engine();
    }
    app.run();
    0
}

impl Target {
    fn new(mtm: MainThreadMarker, controller: Controller) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(controller);
        unsafe { msg_send![super(this), init] }
    }

    fn mtm(&self) -> MainThreadMarker {
        MainThreadMarker::from(self)
    }

    fn as_object(&self) -> &AnyObject {
        self.as_ref()
    }

    /// Common modes, so the title keeps updating while the menu is open.
    fn add_timer(&self, seconds: f64, selector: objc2::runtime::Sel) -> Retained<NSTimer> {
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                seconds,
                self.as_object(),
                selector,
                None,
                true,
            )
        };
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
        timer
    }

    fn schedule_refresh_timer(&self) {
        let c = self.ivars();
        if let Some(old) = c.refresh_timer.borrow_mut().take() {
            old.invalidate();
        }
        let seconds = c.settings.borrow().refresh_interval.max(1) as f64;
        let timer = self.add_timer(seconds, sel!(refreshTick:));
        *c.refresh_timer.borrow_mut() = Some(timer);
    }

    fn rebuild_menu(&self) {
        let c = self.ivars();
        let now = now_secs();
        let snapshot = c.snapshot.borrow();
        let settings = c.settings.borrow();
        let title = format_title(
            snapshot.active_email.as_deref(),
            &snapshot.active_usage,
            &settings,
            now,
            snapshot.active_alias.as_deref(),
        );
        let log_text = std::fs::read_to_string(&c.log_path).unwrap_or_default();
        let history = parse_switch_history(&log_text, SWITCH_HISTORY_LIMIT);
        let openai_enabled = ccswap::openai_mode::is_enabled(&c.paths).unwrap_or(false);
        c.openai_enabled.set(openai_enabled);
        let entries = build_menu(&MenuInput {
            snapshot: &snapshot,
            settings: &settings,
            history: &history,
            threshold: self.threshold(),
            openai_enabled,
            now,
        });
        let mut commands = Vec::new();
        let menu = appkit::build_menu(self.mtm(), &entries, self.as_object(), &mut commands);
        *c.commands.borrow_mut() = commands;
        if let Some(button) = c.status_item.button(self.mtm()) {
            button.setTitle(&NSString::from_str(&title));
        }
        c.status_item.setMenu(Some(&menu));
    }

    fn threshold(&self) -> i64 {
        AutoSwitchSettings::load(&self.ivars().paths).threshold as i64
    }

    fn engine_running(&self) -> bool {
        self.ivars().engine.borrow().is_some()
    }

    /// One snapshot worker at a time; while the engine runs it does the
    /// polling, so the display reads the store only.
    fn refresh_async(&self) {
        let c = self.ivars();
        if c.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let store_only = self.engine_running();
        let source = c.source.clone();
        let refreshing = c.refreshing.clone();
        let logged = c.usage_logged.clone();
        let reports = c.reports.0.clone();
        let env = c.env.clone();
        std::thread::spawn(move || {
            match source.take(store_only) {
                Ok(raw) => {
                    let mut snapshot = MenuSnapshot::adapt(&raw);
                    snapshot.codex = codex_snapshot(&env);
                    log_usage(&snapshot, &logged);
                    let _ = reports.send(Report::Snapshot(snapshot));
                }
                Err(error) => log::debug(format!("menubar snapshot failed: {error}")),
            }
            refreshing.store(false, Ordering::SeqCst);
        });
    }

    fn sync(&self) {
        let c = self.ivars();
        let mut dirty = false;
        let enabled = ccswap::openai_mode::is_enabled(&c.paths).unwrap_or(false);
        dirty |= c.openai_enabled.replace(enabled) != enabled;
        while let Ok(report) = c.reports.1.try_recv() {
            match report {
                Report::Snapshot(snapshot) => {
                    *c.snapshot.borrow_mut() = snapshot;
                    self.write_widget_feed();
                    dirty = true;
                }
                Report::Action {
                    outcome: Err(message),
                } => appkit::alert(self.mtm(), NOTIFICATION_TITLE, &message),
                Report::Action {
                    outcome: Ok(notice),
                } => {
                    if let Some(notice) = notice {
                        appkit::notify(notice.subtitle, &notice.message);
                    }
                    self.refresh_async();
                }
            }
        }
        if dirty {
            self.rebuild_menu();
        }
        self.detect_active_change();
        self.drain_engine_events();
        if let Some(command) = widget::take_request(&c.widget_request_path, now_secs()) {
            self.run_command(command);
        }
    }

    fn write_widget_feed(&self) {
        let c = self.ivars();
        let feed = widget::feed(
            &c.snapshot.borrow(),
            self.threshold(),
            c.settings.borrow().refresh_interval,
            now_secs(),
        );
        let bytes = serde_json::to_vec(&feed).unwrap_or_default();
        let dir = c.actions.backup_dir().join(widget::DIRNAME);
        if let Err(error) = fsutil::ensure_private_dir(&dir)
            .and_then(|()| fsutil::atomic_write_private(&dir, &c.widget_feed_path, &bytes))
        {
            log::debug(format!("widget feed write failed: {error}"));
        }
    }

    /// Picks up switches made anywhere (CLI, Python cswap, the engine)
    /// within a second: a stat per tick, a config read only on change.
    fn detect_active_change(&self) {
        let c = self.ivars();
        if c.refreshing.load(Ordering::SeqCst) {
            return;
        }
        let Some(mtime) = c
            .actions
            .claude_config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
        else {
            return;
        };
        if c.config_mtime.get() == Some(mtime) {
            return;
        }
        c.config_mtime.set(Some(mtime));
        let live = c.actions.live_email();
        if live.is_some() && live != c.snapshot.borrow().active_email {
            self.refresh_async();
        }
    }

    fn drain_engine_events(&self) {
        while let Ok(event) = self.ivars().events.1.try_recv() {
            let subtitle = match &event.kind {
                EventKind::Switch { dry_run: false, .. } => "Auto-switched account",
                EventKind::Quarantined { .. } => "Account quarantined",
                EventKind::AllExhausted { .. } => "All accounts exhausted",
                EventKind::ConfigWarning { .. } => "Configuration warning",
                _ => continue,
            };
            appkit::notify(subtitle, &event.human());
            if matches!(event.kind, EventKind::Switch { .. }) {
                self.refresh_async();
            }
        }
    }

    fn start_engine(&self) {
        let c = self.ivars();
        if self.engine_running() {
            return;
        }
        let handle = LoopHandle::new();
        let (env, paths, events) = (c.env.clone(), c.paths.clone(), c.events.0.clone());
        let thread_handle = handle.clone();
        let spawned = std::thread::Builder::new()
            .name("autoswitch".into())
            .spawn(move || run_engine(env, paths, events, thread_handle));
        match spawned {
            Ok(_) => *c.engine.borrow_mut() = Some(handle),
            Err(error) => {
                log::warning(format!("auto-switch engine failed to start: {error}"));
                appkit::notify("Auto-switch failed to start", &error.to_string());
            }
        }
    }

    fn stop_engine(&self) {
        if let Some(handle) = self.ivars().engine.borrow_mut().take() {
            handle.stop();
        }
    }

    fn save_settings(&self) {
        let c = self.ivars();
        if let Err(error) = c.settings.borrow().save(&c.settings_path) {
            log::warning(format!("Could not save menu bar settings: {error}"));
        }
    }

    fn update_settings(&self, change: impl FnOnce(&mut MenuBarSettings)) {
        change(&mut self.ivars().settings.borrow_mut());
        self.save_settings();
        self.rebuild_menu();
    }

    /// Runs `work` on a worker thread and reports its notice, or its failure as alert text.
    fn spawn_report(&self, work: impl FnOnce() -> Result<Option<Notice>> + Send + 'static) {
        let reports = self.ivars().reports.0.clone();
        std::thread::spawn(move || {
            let outcome = work().map_err(|e| e.to_string());
            let _ = reports.send(Report::Action { outcome });
        });
    }

    fn spawn_action(
        &self,
        notify_switched: bool,
        action: impl FnOnce(&dyn Actions, &mut dyn Console) -> Result<()> + Send + 'static,
    ) {
        let actions = self.ivars().actions.clone();
        self.spawn_report(move || {
            action(actions.as_ref(), &mut SilentConsole)
                .map(|()| notify_switched.then(Notice::switched))
        });
    }

    fn spawn_codex(
        &self,
        action: impl FnOnce(&CodexStore) -> Result<Option<Notice>> + Send + 'static,
    ) {
        let env = self.ivars().env.clone();
        self.spawn_report(move || action(&CodexStore::new(CodexPaths::new(&env))));
    }

    fn codex_switch(&self, number: Option<String>) {
        self.spawn_codex(move |store| {
            store
                .switch(number.as_deref())
                .map(|outcome| outcome.switched.then(|| switch_notice(&outcome)))
        });
    }

    fn codex_login(&self) {
        let running = self.ivars().codex_login_running.clone();
        if running.swap(true, Ordering::SeqCst) {
            appkit::alert(
                self.mtm(),
                NOTIFICATION_TITLE,
                "A Codex login is already waiting for the browser.",
            );
            return;
        }
        appkit::notify(
            "Codex login",
            "Finish logging in in the browser that Codex opened.",
        );
        self.spawn_codex(move |store| {
            let outcome = store.login(LoginStdio::Detached);
            running.store(false, Ordering::SeqCst);
            Ok(Some(login_notice(&outcome?)))
        });
    }

    fn run_command(&self, command: Command) {
        let mtm = self.mtm();
        match command {
            Command::CodexSwitchTo(number) => self.codex_switch(Some(number)),
            Command::CodexSwitchNext => self.codex_switch(None),
            Command::CodexAddFromLogin => {
                self.spawn_codex(|store| store.add_live().map(|o| Some(add_notice(&o))))
            }
            Command::CodexLogin => self.codex_login(),
            Command::CodexRemove(number) => {
                let message = format!("Remove Codex account {number}?");
                if appkit::confirm(mtm, "Remove Codex account", &message, "Remove", "Cancel") {
                    self.spawn_codex(move |store| store.remove(&number).map(|_| None));
                }
            }
            Command::SwitchTo(number) => {
                self.spawn_action(true, move |a, c| a.switch_to(&number, false, c).map(drop))
            }
            Command::Switch(strategy) => self.spawn_action(true, move |a, c| {
                a.switch(strategy.map(SwitchStrategy::new).as_ref(), c)
                    .map(drop)
            }),
            Command::AddFromLogin => self.spawn_action(false, |a, c| a.add_account(None, false, c)),
            Command::AddFromToken => self.add_from_token(),
            Command::SetDisabled { number, disabled } => self.spawn_action(false, move |a, c| {
                a.set_account_disabled(&number, disabled, c)
            }),
            Command::Remove(number) => {
                let message = format!("Remove account {number}?");
                if appkit::confirm(mtm, "Remove account", &message, "Remove", "Cancel") {
                    self.spawn_action(false, move |a, c| a.remove_account(&number, true, c));
                }
            }
            Command::RefreshCredentials => self.refresh_credentials(),
            Command::OpenLog => {
                let c = self.ivars();
                let target = if c.log_path.exists() {
                    c.log_path.clone()
                } else {
                    c.actions.backup_dir()
                };
                let _ = std::process::Command::new("open")
                    .arg("-R")
                    .arg(target)
                    .spawn();
            }
            Command::ToggleAccountName => {
                self.update_settings(|s| s.show_account_name = !s.show_account_name)
            }
            Command::TitlePct(mode) => self.update_settings(|s| s.title_pct = mode.into()),
            Command::ToggleScoped => self.update_settings(|s| s.title_scoped = !s.title_scoped),
            Command::RefreshInterval(secs) => {
                self.ivars().settings.borrow_mut().refresh_interval = secs;
                self.schedule_refresh_timer();
                self.save_settings();
                self.rebuild_menu();
            }
            Command::ToggleAutoSwitch => {
                let enabled = {
                    let mut settings = self.ivars().settings.borrow_mut();
                    settings.auto_switch_enabled = !settings.auto_switch_enabled;
                    settings.auto_switch_enabled
                };
                self.save_settings();
                if enabled {
                    self.start_engine();
                } else {
                    self.stop_engine();
                }
                self.rebuild_menu();
            }
            Command::Threshold(pct) => self.set_threshold(pct),
            Command::RefreshNow => self.refresh_async(),
            Command::ToggleOpenAi => {
                let paths = self.ivars().paths.clone();
                self.spawn_report(move || {
                    let enabled = ccswap::openai_mode::is_enabled(&paths)?;
                    if enabled {
                        ccswap::openai_mode::disable(&paths)?;
                    } else {
                        ccswap::openai_mode::enable(&paths)?;
                    }
                    Ok(None)
                });
            }
            Command::Quit => {
                if let Err(error) = ccswap::openai_mode::disable(&self.ivars().paths) {
                    appkit::alert(mtm, "OpenAI mode", &error.to_string());
                    return;
                }
                self.stop_engine();
                // A LaunchAgent with KeepAlive.SuccessfulExit=false restarts
                // only on a non-zero exit, so quitting must exit 0.
                std::process::exit(0);
            }
        }
    }

    fn add_from_token(&self) {
        let mtm = self.mtm();
        const TITLE: &str = "Add account from setup-token";
        let Some(email) = appkit::prompt(mtm, TITLE, "Email for this token:", "Next") else {
            return;
        };
        let Some(token) = appkit::prompt(mtm, TITLE, "Setup token (sk-ant-oat01-…):", "Add")
        else {
            return;
        };
        self.spawn_action(false, move |a, c| {
            a.add_account_from_token(&token, Some(&email), None, false, c)
        });
    }

    fn refresh_credentials(&self) {
        let c = self.ivars();
        if c.actions.live_email().is_none() {
            appkit::alert(
                self.mtm(),
                NOTIFICATION_TITLE,
                "No active Claude Code login detected. Log in first.",
            );
            return;
        }
        self.spawn_action(false, |a, c| {
            a.add_account(None, false, c).map_err(|e| match e.kind {
                ErrorKind::CredentialReadError => Error::new(e.kind, KEYCHAIN_BLOCKED),
                _ => e,
            })
        });
    }

    fn set_threshold(&self, pct: i64) {
        let c = self.ivars();
        if let Err(error) = Settings::new(&c.paths, ccswap_claude::SETTINGS)
            .set("autoswitch.threshold", &pct.to_string())
        {
            appkit::alert(
                self.mtm(),
                NOTIFICATION_TITLE,
                &format!("Couldn't set threshold: {error}"),
            );
            return;
        }
        if let Some(handle) = c.engine.borrow().as_ref() {
            handle.apply_threshold(pct as f64);
            handle.wake();
        }
        self.rebuild_menu();
    }
}

/// Builds the host inside the thread: the engine borrows it for its life.
fn run_engine(env: Environment, paths: Paths, events: Sender<AutoSwitchEvent>, handle: LoopHandle) {
    let endpoints = Endpoints::new(&env);
    let keychain = Keychain::new(endpoints.security.clone(), &env);
    let secrets = SecretStore::new(keychain, paths.clone(), &env);
    let client = AnthropicClient::new(endpoints);
    let style = Style::new(false, Theme::Dark);
    let host = SwitcherHost::new(&paths, &env, &secrets, &client, &style);
    let mut engine = AutoSwitchEngine::new(
        &host,
        AutoSwitchSettings::load(&paths),
        Box::new(move |event| {
            let _ = events.send(event);
        }),
    )
    .loop_handle(handle);
    engine.run_loop();
}

/// Logs each account's limits when its percentages change, so an idle
/// machine does not fill the log with identical lines.
fn log_usage(snapshot: &MenuSnapshot, logged: &Mutex<UsageLogKeys>) {
    let mut logged = logged.lock().unwrap_or_else(|e| e.into_inner());
    for account in &snapshot.accounts {
        let key = usage_log_key(account.last_good.as_ref());
        if key == (None, None) || logged.get(&account.number) == Some(&key) {
            continue;
        }
        if let Some(line) = format_usage_log(&account.email, account.last_good.as_ref()) {
            log::info(line);
            logged.insert(account.number.clone(), key);
        }
    }
}

/// Empty when Codex has no accounts or the store is unreadable, so the menu shows no Codex section.
fn codex_snapshot(env: &Environment) -> Vec<MenuAccount> {
    let paths = CodexPaths::new(env);
    let store = CodexStore::new(paths.clone());
    let listing = match store.list() {
        Ok(listing) => listing,
        Err(error) => {
            log::debug(format!("menubar Codex listing failed: {error}"));
            return Vec::new();
        }
    };
    let usage = CodexUsage::new(&paths, &Endpoints::new(env)).collect(&store, &listing);
    codex_accounts(&listing, &usage)
}

fn switch_notice(outcome: &SwitchOutcome) -> Notice {
    let mut message = format!(
        "Now on {}. Takes effect in codex sessions started from now on.",
        outcome.to.email
    );
    if outcome.daemon_dir.is_some() {
        message.push_str(" Restart the app-server daemon: codex app-server daemon restart");
    }
    Notice {
        subtitle: "Codex account switched",
        message,
    }
}

fn add_notice(outcome: &AddOutcome) -> Notice {
    let message = match outcome {
        AddOutcome::Added { number, identity } => {
            format!("Added {} as Codex account {number}.", identity.email)
        }
        AddOutcome::AlreadyManaged { number, identity } => {
            format!("{} is already Codex account {number}.", identity.email)
        }
    };
    Notice {
        subtitle: "Codex account",
        message,
    }
}

fn login_notice(outcome: &LoginOutcome) -> Notice {
    let message = match outcome {
        LoginOutcome::Added { number, identity } => {
            format!("Added {} as Codex account {number}.", identity.email)
        }
        LoginOutcome::Updated { number, identity } => format!(
            "{} is already Codex account {number}; updated its stored login.",
            identity.email
        ),
    };
    Notice {
        subtitle: "Codex account",
        message,
    }
}

/// Output has nowhere to go; a prompt reads as end of input.
struct SilentConsole;

impl Console for SilentConsole {
    fn line(&mut self, _text: &str) {}

    fn prompt(&mut self, _question: &str) -> Option<String> {
        None
    }
}
