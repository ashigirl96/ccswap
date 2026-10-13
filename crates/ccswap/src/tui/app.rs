//! The TUI's state machine (upstream `tui/app.py` and the screens of
//! `tui/dashboard.py` / `tui/modals.py`): the snapshot poll loop with its
//! two lanes, the single-flight actions, and every screen's key handling.
//! Blocking work runs on worker threads that report back over a channel.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use ccswap_core::accounts::Console;
use ccswap_core::actions::{ActionResult, Actions, run_action};
use ccswap_core::error::Result as CoreResult;
use ccswap_core::ranking::Strategy;
use ccswap_core::settings::{SettingValue, Settings};
use ccswap_core::snapshot::{AccountsSnapshot, SnapshotSource, account_identity};
use ccswap_core::switcher::SwitchStrategy;
use ccswap_core::usage::now_secs;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use super::data::format_duration;
use super::theme::{Palette, ThemeName};

pub const POLL_INTERVAL_S: f64 = 3.0;
pub const SNAPSHOT_AGE_NOTE_S: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Normal,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Information,
    Warning,
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
}

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
}

/// What the app reads from settings.json at startup.
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

    // -- snapshot poll loop -------------------------------------------------

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
        let numbers: Vec<String> = snap.accounts.iter().map(|a| a.number.clone()).collect();
        let active = active_index(&snap);
        for screen in &mut self.screens {
            if let Screen::Switch(s) = screen
                && s.numbers != numbers
            {
                let first_build = s.numbers.is_empty();
                s.index = if numbers.is_empty() {
                    None
                } else if first_build {
                    Some(active)
                } else {
                    Some(s.index.unwrap_or(0).min(numbers.len() - 1))
                };
                s.numbers = numbers.clone();
            }
        }
        self.snapshot = Some(snap);
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

    /// Applies every finished worker result without waiting.
    pub fn drain(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.handle_message(message);
        }
    }

    /// Waits until no worker is in flight.
    #[cfg(test)]
    pub fn settle(&mut self) {
        while self.in_flight > 0 {
            match self.rx.recv_timeout(Duration::from_secs(5)) {
                Ok(message) => self.handle_message(message),
                Err(_) => panic!("a worker never reported back"),
            }
        }
    }

    fn handle_message(&mut self, message: Message) {
        self.in_flight -= 1;
        match message {
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

    // -- notifications ------------------------------------------------------

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
    }

    // -- actions (single-flight, captured, off-thread) ----------------------

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
            let mut screen = SwitchScreen::default();
            if let Some(snap) = &self.snapshot {
                screen.numbers = snap.accounts.iter().map(|a| a.number.clone()).collect();
                screen.index = (!screen.numbers.is_empty()).then(|| active_index(snap));
            }
            self.push(Screen::Switch(screen));
        }
    }

    fn not_yet(&mut self, view: &str) {
        self.notify_for(
            format!("{view} is not available yet in ccswap"),
            None,
            Severity::Warning,
            5.0,
        );
    }

    // -- theme ----------------------------------------------------------------

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

    // -- screens --------------------------------------------------------------

    fn push(&mut self, screen: Screen) {
        self.screens.push(screen);
    }

    fn pop(&mut self) -> Option<Screen> {
        (self.screens.len() > 1)
            .then(|| self.screens.pop())
            .flatten()
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
            KeyCode::Char('w') => self.not_yet("The watch view"),
            KeyCode::Char('g') => self.not_yet("The auto-switch view"),
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
            "watch" => self.not_yet("The watch view"),
            "auto" => self.not_yet("The auto-switch view"),
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
                let selected = match self.screen() {
                    Screen::Switch(s) => s.index.and_then(|i| s.numbers.get(i)).cloned(),
                    _ => None,
                };
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
                if let Some(s) = self.switch_screen_mut()
                    && let Some(i) = s.index
                {
                    s.index = Some((i + 1).min(s.numbers.len().saturating_sub(1)));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(s) = self.switch_screen_mut()
                    && let Some(i) = s.index
                {
                    s.index = Some(i.saturating_sub(1));
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

/// An integer input accepts an optional leading sign, then digits.
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

fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
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
