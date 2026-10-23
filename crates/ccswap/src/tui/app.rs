//! Blocking work runs on worker threads that report back over a channel.

use ccswap_core::pyjson::py_str;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use ccswap_core::actions::{ActionResult, Actions, run_action};
use ccswap_core::autoswitch::{AutoSwitchSettings, pct_label};
use ccswap_core::console::Console;
use ccswap_core::error::Result as CoreResult;
use ccswap_core::ranking::Strategy;
use ccswap_core::settings::{Kind, SettingValue, Settings, setting_spec};
use ccswap_core::snapshot::{AccountsSnapshot, SnapshotSource, account_identity};
use ccswap_core::switcher::SwitchStrategy;
use ccswap_core::usage::now_secs;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use super::autoview::{LogLine, clock_stamp, event_line};
use super::data::format_duration;
use super::engine::{EngineControl, EngineFactory, EngineReport, EngineStart, idle_factory};
use super::theme::{Palette, ThemeName};

pub const POLL_INTERVAL_S: f64 = 3.0;
pub const SNAPSHOT_AGE_NOTE_S: f64 = 60.0;
/// How long a row whose usage was just re-measured stays highlighted.
pub const FLASH_S: f64 = 1.5;
pub const WATCH_TITLE: &str = "watching all accounts";
pub const WATCH_SELECT_TITLE: &str = "switch to which account? · enter confirm · esc cancel";
const GO_LIVE_MESSAGE: &str = "Go live? claude-swap will switch your active account automatically when the threshold is reached.\n\n(Same behavior as running `cswap auto` in a terminal.)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Normal,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Information,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub message: String,
    pub title: Option<String>,
    pub severity: Severity,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuEntry {
    pub label: String,
    pub action_id: String,
}

impl MenuEntry {
    fn new(label: impl Into<String>, action_id: impl Into<String>) -> Self {
        MenuEntry {
            label: label.into(),
            action_id: action_id.into(),
        }
    }

    pub fn muted(&self) -> bool {
        self.action_id == "back"
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dashboard {
    /// (title, entries); depth 1 is the root menu.
    pub menu_stack: Vec<(String, Vec<MenuEntry>)>,
    pub index: usize,
}

impl Dashboard {
    pub fn entries(&self) -> &[MenuEntry] {
        self.menu_stack.last().map_or(&[], |(_, e)| e)
    }

    pub fn crumb(&self) -> String {
        self.menu_stack
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join(" › ")
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SwitchScreen {
    pub numbers: Vec<String>,
    pub index: Option<usize>,
    /// Each slot's `fetched_at` as of the previous snapshot; empty until
    /// the first one, which never flashes.
    stamps: HashMap<String, Option<f64>>,
    flash_until: HashMap<String, Instant>,
}

impl SwitchScreen {
    fn new(snap: Option<&AccountsSnapshot>, cursor: bool) -> Self {
        let mut list = SwitchScreen::default();
        if let Some(snap) = snap {
            list.apply(snap, cursor);
        }
        list
    }

    /// The cursor lands on the active account only on the first build; later
    /// rebuilds keep it in range.
    fn apply(&mut self, snap: &AccountsSnapshot, cursor: bool) {
        let numbers: Vec<String> = snap.accounts.iter().map(|a| a.number.clone()).collect();
        if numbers != self.numbers {
            let first_build = self.numbers.is_empty();
            self.index = if numbers.is_empty() || !cursor {
                None
            } else if first_build {
                Some(active_index(snap))
            } else {
                Some(self.index.unwrap_or(0).min(numbers.len() - 1))
            };
            self.numbers = numbers;
        }
        let stamps: HashMap<String, Option<f64>> = snap
            .accounts
            .iter()
            .map(|a| (a.number.clone(), a.usage.fetched_at))
            .collect();
        if !self.stamps.is_empty() {
            let until = Instant::now() + Duration::from_secs_f64(FLASH_S);
            for (number, stamp) in &stamps {
                if stamp.is_some() && self.stamps.get(number) != Some(stamp) {
                    self.flash_until.entry(number.clone()).or_insert(until);
                }
            }
        }
        self.stamps = stamps;
    }

    pub fn is_flashing(&self, number: &str, now: Instant) -> bool {
        self.flash_until
            .get(number)
            .is_some_and(|until| *until > now)
    }

    fn expire_flashes(&mut self, now: Instant) {
        self.flash_until.retain(|_, until| *until > now);
    }

    fn cursor_down(&mut self) {
        if let Some(i) = self.index {
            self.index = Some((i + 1).min(self.numbers.len().saturating_sub(1)));
        }
    }

    fn cursor_up(&mut self) {
        if let Some(i) = self.index {
            self.index = Some(i.saturating_sub(1));
        }
    }

    fn selected(&self) -> Option<String> {
        self.index.and_then(|i| self.numbers.get(i)).cloned()
    }
}

/// Every account's full card, hands-off until `s` arms a cursor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WatchScreen {
    pub list: SwitchScreen,
    pub selecting: bool,
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoScreen {
    /// The engine's settings, including a session-only threshold.
    pub settings: AutoSwitchSettings,
    /// settings.json's threshold when the screen opened.
    pub configured_threshold: f64,
    entry_threshold: f64,
    pub adjusting: bool,
    pub dry_run: bool,
    pub log: Vec<LogLine>,
}

const LOG_LINES_KEPT: usize = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct TokenForm {
    pub token: String,
    pub email: Option<String>,
    pub slot: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmPurpose {
    Remove(String),
    AddCurrent,
    Overwrite(TokenForm),
    GoLive,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub title: String,
    pub message: String,
    pub yes_label: String,
    pub focus_yes: bool,
    pub purpose: ConfirmPurpose,
}

/// Focus order: token, email, slot, Add, Cancel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AddTokenModal {
    pub token: String,
    pub email: String,
    pub slot: String,
    pub focus: usize,
    pub error: String,
}

pub const TOKEN_FIELD_COUNT: usize = 3;
const TOKEN_FOCUS_COUNT: usize = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct OutputModal {
    pub title: String,
    pub output: String,
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Dashboard(Dashboard),
    Switch(SwitchScreen),
    Watch(WatchScreen),
    Auto(AutoScreen),
    Confirm(Confirm),
    AddToken(AddTokenModal),
    Output(OutputModal),
}

impl Screen {
    pub fn is_modal(&self) -> bool {
        matches!(
            self,
            Screen::Confirm(_) | Screen::AddToken(_) | Screen::Output(_)
        )
    }
}

type ActionFn = Box<dyn FnOnce(&dyn Actions, &mut dyn Console) -> CoreResult<Option<Value>> + Send>;

enum Message {
    Snapshot {
        generation: u64,
        lane: Lane,
        result: Result<AccountsSnapshot, String>,
    },
    ActionDone {
        label: String,
        result: ActionResult,
        show_output: bool,
    },
    Engine(EngineReport),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppConfig {
    pub threshold: Option<f64>,
    pub theme: ThemeName,
    /// The sensed terminal background: `Some(true)` for light.
    pub detected_light: Option<bool>,
}

impl AppConfig {
    /// Missing or invalid settings fall back to the defaults.
    pub fn load(settings: &Settings) -> Self {
        let threshold = match settings.get("autoswitch.threshold").map(|s| s.value) {
            Ok(SettingValue::Float(f)) => Some(f),
            Ok(SettingValue::Int(i)) => Some(i as f64),
            _ => None,
        };
        let theme = match settings.get("ui.theme").map(|s| s.value) {
            Ok(SettingValue::Text(name)) => ThemeName::parse(&name).unwrap_or(ThemeName::Auto),
            _ => ThemeName::Auto,
        };
        AppConfig {
            threshold,
            theme,
            detected_light: None,
        }
    }
}

pub struct App {
    actions: Arc<dyn Actions>,
    source: Arc<SnapshotSource>,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    in_flight: usize,
    pub snapshot: Option<AccountsSnapshot>,
    pub refresh_status: String,
    pub busy: bool,
    store_only: bool,
    full_next: bool,
    normal_refreshing: bool,
    store_refreshing: bool,
    normal_started_at: Option<f64>,
    refresh_generation: u64,
    applied_generation: u64,
    last_refresh_error: String,
    pub threshold_pct: Option<f64>,
    pub theme_name: ThemeName,
    detected_light: Option<bool>,
    pub screens: Vec<Screen>,
    pub notifications: Vec<Notification>,
    pub should_quit: bool,
    engine_factory: EngineFactory,
    engine: Option<Box<dyn EngineControl>>,
}

impl App {
    pub fn new(actions: Arc<dyn Actions>, config: AppConfig) -> Self {
        let (tx, rx) = channel();
        let dashboard = Dashboard {
            menu_stack: vec![("menu".into(), root_entries())],
            index: 0,
        };
        App {
            source: Arc::new(SnapshotSource::new(actions.clone())),
            actions,
            tx,
            rx,
            in_flight: 0,
            snapshot: None,
            refresh_status: String::new(),
            busy: false,
            store_only: false,
            full_next: false,
            normal_refreshing: false,
            store_refreshing: false,
            normal_started_at: None,
            refresh_generation: 0,
            applied_generation: 0,
            last_refresh_error: String::new(),
            threshold_pct: config.threshold,
            theme_name: config.theme,
            detected_light: config.detected_light,
            screens: vec![Screen::Dashboard(dashboard)],
            notifications: Vec::new(),
            should_quit: false,
            engine_factory: idle_factory(),
            engine: None,
        }
    }

    pub fn with_engine_factory(mut self, factory: EngineFactory) -> Self {
        self.engine_factory = factory;
        self
    }

    pub fn shutdown(&mut self) {
        if let Some(engine) = self.engine.take() {
            engine.stop();
        }
    }

    pub fn palette(&self) -> Palette {
        self.theme_name.palette(self.detected_light)
    }

    pub fn screen(&self) -> &Screen {
        self.screens.last().expect("the dashboard is never popped")
    }

    pub fn dashboard(&self) -> &Dashboard {
        match &self.screens[0] {
            Screen::Dashboard(d) => d,
            _ => unreachable!("the bottom screen is the dashboard"),
        }
    }

    fn dashboard_mut(&mut self) -> &mut Dashboard {
        match &mut self.screens[0] {
            Screen::Dashboard(d) => d,
            _ => unreachable!("the bottom screen is the dashboard"),
        }
    }

    /// One poll tick: the network lane when it is idle, else a store-only
    /// repaint so other processes' writes still show while a fetch blocks.
    pub fn tick(&mut self) {
        if self.store_only {
            self.start_store_refresh();
        } else if !self.normal_refreshing {
            self.full_next = false;
            self.start_normal_refresh();
        } else {
            self.start_store_refresh();
        }
    }

    fn start_normal_refresh(&mut self) {
        if self.normal_refreshing {
            return;
        }
        self.normal_refreshing = true;
        self.normal_started_at = Some(now_secs());
        let generation = self.next_generation();
        self.update_refresh_status();
        self.spawn_refresh(generation, Lane::Normal);
    }

    fn start_store_refresh(&mut self) {
        if self.store_refreshing {
            return;
        }
        self.store_refreshing = true;
        let generation = self.next_generation();
        self.update_refresh_status();
        self.spawn_refresh(generation, Lane::Store);
    }

    fn next_generation(&mut self) -> u64 {
        self.refresh_generation += 1;
        self.refresh_generation
    }

    fn spawn_refresh(&mut self, generation: u64, lane: Lane) {
        let (source, tx) = (self.source.clone(), self.tx.clone());
        self.in_flight += 1;
        std::thread::spawn(move || {
            let result = source.take(lane == Lane::Store).map_err(|e| e.to_string());
            let _ = tx.send(Message::Snapshot {
                generation,
                lane,
                result,
            });
        });
    }

    fn apply_snapshot(&mut self, generation: u64, lane: Lane, snap: AccountsSnapshot) {
        self.lane_finished(lane);
        self.last_refresh_error.clear();
        if generation >= self.applied_generation {
            self.applied_generation = generation;
            self.on_snapshot(snap);
        } else if let Some(current) = self.snapshot.take() {
            self.on_snapshot(merge_late(current, &snap));
        }
        self.update_refresh_status();
    }

    fn lane_finished(&mut self, lane: Lane) {
        match lane {
            Lane::Normal => {
                self.normal_refreshing = false;
                self.normal_started_at = None;
            }
            Lane::Store => self.store_refreshing = false,
        }
    }

    fn refresh_failed(&mut self, lane: Lane, msg: String) {
        self.lane_finished(lane);
        self.update_refresh_status();
        if msg != self.last_refresh_error {
            self.last_refresh_error = msg.clone();
            let label = match lane {
                Lane::Normal => "Refresh",
                Lane::Store => "Store refresh",
            };
            self.notify_for(
                format!("{label} failed: {msg}"),
                None,
                Severity::Warning,
                6.0,
            );
        }
    }

    fn on_snapshot(&mut self, snap: AccountsSnapshot) {
        for screen in &mut self.screens {
            match screen {
                Screen::Switch(s) => s.apply(&snap, true),
                Screen::Watch(w) => w.list.apply(&snap, w.selecting),
                _ => {}
            }
        }
        self.snapshot = Some(snap);
    }

    /// Auto screen: the engine is the only fetcher, the poller reads the store.
    pub fn set_store_only(&mut self, value: bool) {
        self.store_only = value;
        self.request_refresh(false);
    }

    #[cfg(test)]
    pub fn store_only(&self) -> bool {
        self.store_only
    }

    pub fn update_refresh_status(&mut self) {
        let now = now_secs();
        let mut parts = Vec::new();
        if let Some(snap) = &self.snapshot {
            let age = (now - snap.taken_at).max(0.0);
            if age >= SNAPSHOT_AGE_NOTE_S {
                parts.push(format!("snapshot {} ago", format_duration(age)));
            }
        }
        if let Some(started) = self.normal_started_at.filter(|_| self.normal_refreshing) {
            let elapsed = now - started;
            if elapsed >= POLL_INTERVAL_S {
                parts.push(format!("refreshing {}", format_duration(elapsed)));
            }
        }
        self.refresh_status = parts.join(" · ");
    }

    pub fn request_refresh(&mut self, full: bool) {
        if full {
            self.full_next = true;
        }
        self.tick();
    }

    pub fn drain(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.handle_message(message);
        }
    }

    #[cfg(test)]
    pub fn settle(&mut self) {
        while self.in_flight > 0 {
            match self.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(message) => self.handle_message(message),
                Err(_) => panic!("a worker never reported back"),
            }
        }
    }

    #[cfg(test)]
    pub fn pretend_normal_refresh_running_for(&mut self, seconds: f64) {
        self.normal_refreshing = true;
        self.normal_started_at = Some(now_secs() - seconds);
    }

    #[cfg(test)]
    pub fn wait_until(&mut self, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(message) => self.handle_message(message),
                Err(_) => panic!("the condition never held"),
            }
        }
    }

    fn handle_message(&mut self, message: Message) {
        if !matches!(message, Message::Engine(_)) {
            self.in_flight -= 1;
        }
        match message {
            Message::Engine(report) => self.on_engine_report(report),
            Message::Snapshot {
                generation,
                lane,
                result: Ok(snap),
            } => self.apply_snapshot(generation, lane, snap),
            Message::Snapshot {
                lane,
                result: Err(msg),
                ..
            } => self.refresh_failed(lane, msg),
            Message::ActionDone {
                label,
                result,
                show_output,
            } => self.action_done(&label, result, show_output),
        }
    }

    pub fn notify(&mut self, message: impl Into<String>) {
        self.notify_for(message, None, Severity::Information, 5.0);
    }

    fn notify_for(
        &mut self,
        message: impl Into<String>,
        title: Option<&str>,
        severity: Severity,
        timeout_s: f64,
    ) {
        self.notifications.push(Notification {
            message: message.into(),
            title: title.map(str::to_string),
            severity,
            expires_at: Instant::now() + Duration::from_secs_f64(timeout_s),
        });
    }

    pub fn expire_notifications(&mut self, now: Instant) {
        self.notifications.retain(|n| n.expires_at > now);
        for screen in &mut self.screens {
            match screen {
                Screen::Switch(s) => s.expire_flashes(now),
                Screen::Watch(w) => w.list.expire_flashes(now),
                _ => {}
            }
        }
    }

    fn start_action(&mut self, label: String, f: ActionFn, show_output: bool) {
        if self.busy {
            self.notify_for(
                "Another action is still running",
                None,
                Severity::Warning,
                5.0,
            );
            return;
        }
        self.busy = true;
        self.in_flight += 1;
        let (actions, tx) = (self.actions.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let result = run_action(|console| f(actions.as_ref(), console));
            let _ = tx.send(Message::ActionDone {
                label,
                result,
                show_output,
            });
        });
    }

    fn action_done(&mut self, label: &str, result: ActionResult, show_output: bool) {
        self.busy = false;
        self.request_refresh(false);
        if !result.ok {
            self.push(Screen::Output(OutputModal {
                title: format!("{label} — failed"),
                output: result.output_text(),
                scroll: 0,
            }));
            return;
        }
        if let Some(payload) = result
            .payload
            .as_ref()
            .filter(|p| p.get("switched").is_some())
        {
            if payload
                .get("switched")
                .is_some_and(ccswap_core::usage::truthy)
            {
                let to = payload.get("to");
                let email = to
                    .and_then(|t| t.get("email"))
                    .and_then(Value::as_str)
                    .filter(|e| !e.is_empty());
                let target = match email {
                    Some(email) => email.to_string(),
                    None => format!(
                        "account {}",
                        to.and_then(|t| t.get("number"))
                            .map_or("None".into(), py_str)
                    ),
                };
                self.notify_for(
                    format!("Switched to {target}"),
                    Some("Switch"),
                    Severity::Information,
                    5.0,
                );
            } else {
                let reason = payload
                    .get("reason")
                    .filter(|r| ccswap_core::usage::truthy(r))
                    .map_or("no switch performed".into(), py_str);
                self.notify_for(reason, Some("No switch"), Severity::Warning, 5.0);
            }
            return;
        }
        let output = result.output_text();
        if show_output && !output.trim().is_empty() {
            self.push(Screen::Output(OutputModal {
                title: label.to_string(),
                output,
                scroll: 0,
            }));
        } else {
            let first = result.first_line();
            if !first.is_empty() {
                self.notify(first);
            }
        }
    }

    pub fn do_switch(&mut self, number: &str) {
        let target = number.to_string();
        self.start_action(
            format!("Switch to account {number}"),
            Box::new(move |a, c| a.switch_to(&target, false, c)),
            false,
        );
    }

    pub fn switch_best(&mut self) {
        self.start_action(
            "Switch (best)".into(),
            Box::new(|a, c| a.switch(Some(&SwitchStrategy::new(Strategy::Best)), c)),
            false,
        );
    }

    pub fn do_toggle_disabled(&mut self, number: &str) {
        let Some(acc) = self
            .snapshot
            .as_ref()
            .and_then(|s| s.accounts.iter().find(|a| a.number == number))
        else {
            return;
        };
        let target = !acc.disabled;
        let verb = if target { "Disable" } else { "Enable" };
        let num = number.to_string();
        self.start_action(
            format!("{verb} account {number}"),
            Box::new(move |a, c| a.set_account_disabled(&num, target, c).map(|()| None)),
            false,
        );
    }

    pub fn confirm_remove(&mut self, number: &str, email: &str) {
        self.push(Screen::Confirm(Confirm {
            title: "Remove account".into(),
            message: format!(
                "Remove account {number} ({email})?\n\nIts stored credentials and config backup are deleted."
            ),
            yes_label: "Remove".into(),
            focus_yes: true,
            purpose: ConfirmPurpose::Remove(number.to_string()),
        }));
    }

    pub fn add_current(&mut self) {
        self.push(Screen::Confirm(Confirm {
            title: "Add account".into(),
            message: "Back up the current Claude Code login as a managed account?\n\nIf this account is already managed, its stored credentials are refreshed in place.".into(),
            yes_label: "Add".into(),
            focus_yes: true,
            purpose: ConfirmPurpose::AddCurrent,
        }));
    }

    pub fn add_token(&mut self) {
        self.push(Screen::AddToken(AddTokenModal::default()));
    }

    fn on_confirm(&mut self, purpose: ConfirmPurpose) {
        match purpose {
            ConfirmPurpose::Remove(number) => {
                let num = number.clone();
                self.start_action(
                    format!("Remove account {number}"),
                    Box::new(move |a, c| a.remove_account(&num, true, c).map(|()| None)),
                    false,
                );
            }
            ConfirmPurpose::AddCurrent => self.start_action(
                "Add current login".into(),
                Box::new(|a, c| a.add_account(None, false, c).map(|()| None)),
                true,
            ),
            ConfirmPurpose::Overwrite(form) => self.run_add_token(form),
            ConfirmPurpose::GoLive => self.restart_engine(false),
        }
    }

    fn on_token_form(&mut self, form: TokenForm) {
        match self.slot_occupant(form.slot) {
            Some(occupant) => self.push(Screen::Confirm(Confirm {
                title: "Overwrite slot".into(),
                message: format!(
                    "Slot {} is occupied by {occupant}. Overwrite?",
                    form.slot.unwrap_or_default()
                ),
                yes_label: "Overwrite".into(),
                focus_yes: true,
                purpose: ConfirmPurpose::Overwrite(form),
            })),
            None => self.run_add_token(form),
        }
    }

    fn run_add_token(&mut self, form: TokenForm) {
        self.start_action(
            "Add account from token".into(),
            Box::new(move |a, c| {
                a.add_account_from_token(&form.token, form.email.as_deref(), form.slot, true, c)
                    .map(|()| None)
            }),
            true,
        );
    }

    fn slot_occupant(&self, slot: Option<i64>) -> Option<String> {
        let slot = slot?.to_string();
        self.snapshot
            .as_ref()?
            .accounts
            .iter()
            .find(|a| a.number == slot)
            .map(|a| a.email.clone())
    }

    pub fn refresh_full(&mut self) {
        self.request_refresh(true);
        self.notify_for("Refreshing usage…", None, Severity::Information, 2.0);
    }

    fn open_switch(&mut self) {
        if !matches!(self.screen(), Screen::Switch(_)) {
            let screen = SwitchScreen::new(self.snapshot.as_ref(), true);
            self.push(Screen::Switch(screen));
        }
    }

    pub fn open_watch(&mut self) {
        if !matches!(self.screen(), Screen::Watch(_)) {
            self.push(Screen::Watch(WatchScreen {
                list: SwitchScreen::new(self.snapshot.as_ref(), false),
                ..WatchScreen::default()
            }));
        }
    }

    pub fn watch_title(&self, watch: &WatchScreen) -> String {
        if watch.selecting {
            WATCH_SELECT_TITLE.into()
        } else if self.refresh_status.is_empty() {
            WATCH_TITLE.into()
        } else {
            format!("{WATCH_TITLE} · {}", self.refresh_status)
        }
    }

    fn watch_mut(&mut self) -> Option<&mut WatchScreen> {
        match self.screens.last_mut() {
            Some(Screen::Watch(w)) => Some(w),
            _ => None,
        }
    }

    fn set_selecting(&mut self, on: bool) {
        let active = self.snapshot.as_ref().map(active_index);
        let Some(watch) = self.watch_mut() else {
            return;
        };
        watch.selecting = on;
        watch.list.index = match active {
            Some(i) if on && !watch.list.numbers.is_empty() => Some(i),
            _ => None,
        };
    }

    /// Opens on a dry-run engine: going live is a separate, confirmed step.
    pub fn open_auto(&mut self) {
        if self.screens.iter().any(|s| matches!(s, Screen::Auto(_))) {
            return;
        }
        let settings =
            AutoSwitchSettings::from_settings(&Settings::in_dir(&self.actions.backup_dir()));
        // Bars and engine agree on the file's current value.
        self.threshold_pct = Some(settings.threshold);
        self.push(Screen::Auto(AutoScreen {
            configured_threshold: settings.threshold,
            entry_threshold: settings.threshold,
            settings,
            adjusting: false,
            dry_run: true,
            log: Vec::new(),
        }));
        self.set_store_only(true);
        self.start_engine(true);
    }

    pub fn auto_screen(&self) -> Option<&AutoScreen> {
        self.screens.iter().rev().find_map(|s| match s {
            Screen::Auto(a) => Some(a),
            _ => None,
        })
    }

    fn auto_mut(&mut self) -> Option<&mut AutoScreen> {
        self.screens.iter_mut().rev().find_map(|s| match s {
            Screen::Auto(a) => Some(a),
            _ => None,
        })
    }

    fn log(&mut self, line: LogLine) {
        if let Some(auto) = self.auto_mut() {
            auto.log.push(line);
            let excess = auto.log.len().saturating_sub(LOG_LINES_KEPT);
            auto.log.drain(..excess);
        }
    }

    fn start_engine(&mut self, dry_run: bool) {
        let Some(settings) = self.auto_screen().map(|a| a.settings.clone()) else {
            return;
        };
        let tx = self.tx.clone();
        let control = (self.engine_factory)(EngineStart {
            settings,
            dry_run,
            sink: Arc::new(move |report| {
                let _ = tx.send(Message::Engine(report));
            }),
        });
        self.engine = Some(control);
        if let Some(auto) = self.auto_mut() {
            auto.dry_run = dry_run;
        }
        let mode = if dry_run {
            "DRY-RUN (watching only)"
        } else {
            "LIVE (will switch accounts)"
        };
        self.log(LogLine::note(format!("— engine started: {mode} —")));
    }

    fn restart_engine(&mut self, dry_run: bool) {
        if let Some(engine) = self.engine.take() {
            engine.stop();
        }
        self.start_engine(dry_run);
    }

    fn on_engine_report(&mut self, report: EngineReport) {
        if self.auto_screen().is_none() {
            return;
        }
        match report {
            EngineReport::Event(event) => {
                self.log(event_line(&event, clock_stamp()));
                if event.name() == "switch" {
                    self.request_refresh(false);
                }
            }
            EngineReport::Crashed(message) => self.notify_for(
                format!("Auto-switch engine stopped: {message}"),
                None,
                Severity::Error,
                10.0,
            ),
        }
    }

    /// A session threshold must not outlive the engine it steered.
    fn close_auto(&mut self, auto: &AutoScreen) {
        self.shutdown();
        self.actions.clear_poll_policy_inputs();
        self.threshold_pct = Some(auto.configured_threshold);
        self.set_store_only(false);
    }

    fn toggle_live(&mut self) {
        let Some(dry_run) = self.auto_screen().map(|a| a.dry_run) else {
            return;
        };
        if dry_run {
            self.push(Screen::Confirm(Confirm {
                title: "Go live".into(),
                message: GO_LIVE_MESSAGE.into(),
                yes_label: "Go live".into(),
                focus_yes: true,
                purpose: ConfirmPurpose::GoLive,
            }));
        } else {
            self.restart_engine(true);
        }
    }

    fn toggle_adjust(&mut self) {
        let Some(auto) = self.auto_mut() else {
            return;
        };
        if auto.adjusting {
            self.end_adjust();
        } else {
            auto.adjusting = true;
            auto.entry_threshold = auto.settings.threshold;
        }
    }

    fn end_adjust(&mut self) {
        let Some(auto) = self.auto_mut() else {
            return;
        };
        auto.adjusting = false;
        let threshold = auto.settings.threshold;
        if threshold == auto.entry_threshold {
            return;
        }
        if let Some(engine) = &self.engine {
            engine.wake();
        }
        self.log(LogLine::note(format!(
            "— threshold set to {}% for this session —",
            pct_label(threshold)
        )));
    }

    /// Session-only: settings.json is never written.
    pub fn threshold_step(&mut self, delta: f64) {
        let Some(auto) = self.auto_screen().filter(|a| a.adjusting) else {
            return;
        };
        let (lo, hi) = match setting_spec("autoswitch.threshold").map(|s| s.kind) {
            Ok(Kind::Float { lo, hi }) => (lo, hi),
            _ => (f64::MIN, f64::MAX),
        };
        let value = (auto.settings.threshold + delta).max(lo).min(hi);
        if value == auto.settings.threshold {
            return;
        }
        let models = auto.settings.models();
        if let Some(auto) = self.auto_mut() {
            auto.settings.threshold = value;
        }
        if let Some(engine) = &self.engine {
            engine.apply_threshold(value);
        }
        self.actions.set_poll_policy_inputs(value, models);
        self.threshold_pct = Some(value);
    }

    pub fn apply_theme(&mut self, name: ThemeName) {
        self.theme_name = name;
        let settings = Settings::in_dir(&self.actions.backup_dir());
        if let Err(e) = settings.set("ui.theme", name.as_str()) {
            self.notify_for(
                format!("Could not save theme: {e}"),
                None,
                Severity::Warning,
                5.0,
            );
        }
    }

    fn toggle_theme(&mut self) {
        let next = self.theme_name.next();
        self.apply_theme(next);
        self.notify(format!("Theme: {}", next.as_str()));
    }

    fn push(&mut self, screen: Screen) {
        self.screens.push(screen);
    }

    fn pop(&mut self) -> Option<Screen> {
        let screen = (self.screens.len() > 1)
            .then(|| self.screens.pop())
            .flatten();
        if let Some(Screen::Auto(auto)) = &screen {
            self.close_auto(auto);
        }
        screen
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('t') {
            self.toggle_theme();
            return;
        }
        match self.screens.len() - 1 {
            0 => self.dashboard_key(key),
            _ => match self.screen().clone() {
                Screen::Switch(_) => self.switch_key(key),
                Screen::Watch(_) => self.watch_key(key),
                Screen::Auto(_) => self.auto_key(key),
                Screen::Confirm(c) => self.confirm_key(key, c),
                Screen::AddToken(_) => self.token_key(key),
                Screen::Output(_) => self.output_key(key),
                Screen::Dashboard(_) => unreachable!("the dashboard is only at the bottom"),
            },
        }
    }

    fn dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('s') => self.open_switch(),
            KeyCode::Char('w') => self.open_watch(),
            KeyCode::Char('g') => self.open_auto(),
            KeyCode::Esc | KeyCode::Left => self.pop_menu(),
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('f') => self.refresh_full(),
            KeyCode::Char('j') | KeyCode::Down => {
                let d = self.dashboard_mut();
                d.index = (d.index + 1).min(d.entries().len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let d = self.dashboard_mut();
                d.index = d.index.saturating_sub(1);
            }
            KeyCode::Enter => {
                let d = self.dashboard();
                if let Some(entry) = d.entries().get(d.index) {
                    let id = entry.action_id.clone();
                    self.dispatch(&id);
                }
            }
            _ => {}
        }
    }

    fn push_menu(&mut self, title: &str, entries: Vec<MenuEntry>) {
        let d = self.dashboard_mut();
        d.menu_stack.push((title.into(), entries));
        d.index = 0;
    }

    fn pop_menu(&mut self) {
        let d = self.dashboard_mut();
        if d.menu_stack.len() > 1 {
            d.menu_stack.pop();
            d.index = 0;
        }
    }

    fn dispatch(&mut self, action_id: &str) {
        match action_id {
            "back" => self.pop_menu(),
            "switch" => self.open_switch(),
            "watch" => self.open_watch(),
            "auto" => self.open_auto(),
            "add-login" => self.add_current(),
            "add-token" => self.add_token(),
            "quit" => self.should_quit = true,
            "add-menu" => self.push_menu("add account", add_entries()),
            "remove-menu" => {
                let entries = remove_entries(self.snapshot.as_ref());
                self.push_menu("remove account", entries);
            }
            "disable-menu" => {
                let entries = disable_entries(self.snapshot.as_ref());
                self.push_menu("disable / enable", entries);
            }
            "theme-menu" => {
                let entries = theme_entries(self.theme_name);
                self.push_menu("theme", entries);
            }
            id => {
                if let Some(number) = id.strip_prefix("remove:") {
                    let email = self
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.accounts.iter().find(|a| a.number == number))
                        .map_or("?".into(), |a| a.email.clone());
                    self.confirm_remove(number, &email);
                } else if let Some(name) = id.strip_prefix("theme:") {
                    if let Some(theme) = ThemeName::parse(name) {
                        self.apply_theme(theme);
                        self.notify(format!("Theme: {name}"));
                    }
                    self.pop_menu();
                } else if let Some(number) = id.strip_prefix("disable:") {
                    self.do_toggle_disabled(number);
                    self.pop_menu();
                }
            }
        }
    }

    fn switch_screen_mut(&mut self) -> Option<&mut SwitchScreen> {
        match self.screens.last_mut() {
            Some(Screen::Switch(s)) => Some(s),
            _ => None,
        }
    }

    fn switch_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let selected = self.switch_screen_mut().and_then(|s| s.selected());
                if let Some(number) = selected {
                    self.do_switch(&number);
                    self.pop();
                }
            }
            KeyCode::Char('b') => self.switch_best(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {
                self.pop();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(s) = self.switch_screen_mut() {
                    s.cursor_down();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(s) = self.switch_screen_mut() {
                    s.cursor_up();
                }
            }
            _ => {}
        }
    }

    /// Enter switches and stays here; Esc disarms selection before leaving.
    fn watch_key(&mut self, key: KeyEvent) {
        let Some(selecting) = self.watch_mut().map(|w| w.selecting) else {
            return;
        };
        match key.code {
            KeyCode::Char('s') => self.set_selecting(!selecting),
            KeyCode::Enter if selecting => {
                if let Some(number) = self.watch_mut().and_then(|w| w.list.selected()) {
                    self.do_switch(&number);
                    self.set_selecting(false);
                }
            }
            KeyCode::Char('f') => self.refresh_full(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if selecting {
                    self.set_selecting(false);
                } else {
                    self.pop();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(w) = self.watch_mut() {
                    if selecting {
                        w.list.cursor_down();
                    } else {
                        w.scroll = w.scroll.saturating_add(1);
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(w) = self.watch_mut() {
                    if selecting {
                        w.list.cursor_up();
                    } else {
                        w.scroll = w.scroll.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
    }

    /// Arrows and Enter are inert until `t` arms threshold adjusting; Esc
    /// leaves adjusting before the screen.
    fn auto_key(&mut self, key: KeyEvent) {
        let adjusting = self.auto_screen().is_some_and(|a| a.adjusting);
        match key.code {
            KeyCode::Char('l') => self.toggle_live(),
            KeyCode::Char('t') => self.toggle_adjust(),
            KeyCode::Left if adjusting => self.threshold_step(-1.0),
            KeyCode::Right if adjusting => self.threshold_step(1.0),
            KeyCode::Enter if adjusting => self.end_adjust(),
            KeyCode::Esc | KeyCode::Char('q') => {
                if adjusting {
                    self.end_adjust();
                } else {
                    self.pop();
                }
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, key: KeyEvent, confirm: Confirm) {
        let answer = match key.code {
            KeyCode::Char('y') => Some(true),
            KeyCode::Char('n') | KeyCode::Esc => Some(false),
            KeyCode::Enter => Some(confirm.focus_yes),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                if let Some(Screen::Confirm(c)) = self.screens.last_mut() {
                    c.focus_yes = !c.focus_yes;
                }
                None
            }
            _ => None,
        };
        if let Some(yes) = answer {
            self.pop();
            if yes {
                self.on_confirm(confirm.purpose);
            }
        }
    }

    fn token_modal_mut(&mut self) -> Option<&mut AddTokenModal> {
        match self.screens.last_mut() {
            Some(Screen::AddToken(m)) => Some(m),
            _ => None,
        }
    }

    fn token_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.token_modal_mut() else {
            return;
        };
        let on_button = modal.focus >= TOKEN_FIELD_COUNT;
        match key.code {
            KeyCode::Esc => {
                self.pop();
            }
            KeyCode::Tab | KeyCode::Down => modal.focus = (modal.focus + 1) % TOKEN_FOCUS_COUNT,
            KeyCode::BackTab | KeyCode::Up => {
                modal.focus = (modal.focus + TOKEN_FOCUS_COUNT - 1) % TOKEN_FOCUS_COUNT
            }
            KeyCode::Right if on_button => modal.focus = (modal.focus + 1) % TOKEN_FOCUS_COUNT,
            KeyCode::Left if on_button => {
                modal.focus = (modal.focus + TOKEN_FOCUS_COUNT - 1) % TOKEN_FOCUS_COUNT
            }
            KeyCode::Enter if modal.focus == TOKEN_FIELD_COUNT + 1 => {
                self.pop();
            }
            KeyCode::Enter => self.submit_token(),
            KeyCode::Backspace if !on_button => {
                field_mut(modal).pop();
            }
            KeyCode::Char(c) if !on_button => {
                let slot_field = modal.focus == 2;
                let field = field_mut(modal);
                if !slot_field || accepts_integer_char(field, c) {
                    field.push(c);
                }
            }
            _ => {}
        }
    }

    fn submit_token(&mut self) {
        let Some(modal) = self.token_modal_mut() else {
            return;
        };
        match token_form(modal) {
            Ok(form) => {
                self.pop();
                self.on_token_form(form);
            }
            Err(message) => modal.error = message.into(),
        }
    }

    fn output_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                self.pop();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(Screen::Output(o)) = self.screens.last_mut() {
                    o.scroll = o.scroll.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(Screen::Output(o)) = self.screens.last_mut() {
                    o.scroll = o.scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
    }
}

fn field_mut(modal: &mut AddTokenModal) -> &mut String {
    match modal.focus {
        0 => &mut modal.token,
        1 => &mut modal.email,
        _ => &mut modal.slot,
    }
}

fn accepts_integer_char(current: &str, c: char) -> bool {
    c.is_ascii_digit() || (current.is_empty() && (c == '-' || c == '+'))
}

fn token_form(modal: &AddTokenModal) -> Result<TokenForm, &'static str> {
    let token = modal.token.trim();
    if token.is_empty() {
        return Err("Token is required.");
    }
    let email = Some(modal.email.trim().to_string()).filter(|e| !e.is_empty());
    let slot_raw = modal.slot.trim();
    let slot = if slot_raw.is_empty() {
        None
    } else {
        let slot: i64 = slot_raw.parse().map_err(|_| "Slot must be a number.")?;
        if slot < 1 {
            return Err("Slot must be >= 1.");
        }
        Some(slot)
    };
    Ok(TokenForm {
        token: token.to_string(),
        email,
        slot,
    })
}

fn active_index(snap: &AccountsSnapshot) -> usize {
    snap.accounts
        .iter()
        .position(|a| Some(&a.number) == snap.active_number.as_ref())
        .unwrap_or(0)
}

/// An older pass finishing after a newer one: keep the newer metadata, but
/// take its per-account usage where the account is unchanged (the source
/// already rejected usage regressions).
pub fn merge_late(mut current: AccountsSnapshot, late: &AccountsSnapshot) -> AccountsSnapshot {
    for acc in &mut current.accounts {
        if let Some(other) = late.accounts.iter().find(|o| o.number == acc.number)
            && account_identity(acc) == account_identity(other)
        {
            acc.usage = other.usage.clone();
        }
    }
    current.taken_at = current.taken_at.max(late.taken_at);
    current
}

pub fn root_entries() -> Vec<MenuEntry> {
    vec![
        MenuEntry::new("Switch account…", "switch"),
        MenuEntry::new("Watch accounts", "watch"),
        MenuEntry::new("Auto-switch view", "auto"),
        MenuEntry::new("Add account…", "add-menu"),
        MenuEntry::new("Disable / enable account…", "disable-menu"),
        MenuEntry::new("Remove account…", "remove-menu"),
        MenuEntry::new("Theme…", "theme-menu"),
        MenuEntry::new("Quit", "quit"),
    ]
}

fn back() -> MenuEntry {
    MenuEntry::new("← back", "back")
}

fn add_entries() -> Vec<MenuEntry> {
    vec![
        MenuEntry::new("From current Claude Code login", "add-login"),
        MenuEntry::new("From a setup-token / API key…", "add-token"),
        back(),
    ]
}

fn remove_entries(snap: Option<&AccountsSnapshot>) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = snap
        .map(|s| s.accounts.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|acc| {
            MenuEntry::new(
                format!(
                    "{}  {}  [{}]",
                    acc.number,
                    acc.display_name(),
                    acc.display_tag()
                ),
                format!("remove:{}", acc.number),
            )
        })
        .collect();
    entries.push(back());
    entries
}

fn disable_entries(snap: Option<&AccountsSnapshot>) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = snap
        .map(|s| s.accounts.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|acc| {
            let (state, action) = if acc.disabled {
                ("  (disabled)", "→ enable")
            } else {
                ("", "→ disable")
            };
            MenuEntry::new(
                format!("{}  {}{state}   {action}", acc.number, acc.display_name()),
                format!("disable:{}", acc.number),
            )
        })
        .collect();
    entries.push(back());
    entries
}

fn theme_entries(current: ThemeName) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = ThemeName::ALL
        .iter()
        .map(|&name| {
            let marker = if name == current { '●' } else { ' ' };
            MenuEntry::new(
                format!("{marker} {}", name.as_str()),
                format!("theme:{}", name.as_str()),
            )
        })
        .collect();
    entries.push(back());
    entries
}
