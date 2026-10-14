//! Test doubles and builders (upstream test_tui.py `FakeSwitcher`,
//! `make_entry`, `make_account`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};

use ccswap_core::accounts::Console;
use ccswap_core::actions::Actions;
use ccswap_core::autoswitch::{AutoSwitchEvent, AutoSwitchSettings, EventKind};
use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::settings::Settings;
use ccswap_core::snapshot::{AccountKind, AccountSnapshot, AccountsSnapshot};
use ccswap_core::switcher::SwitchStrategy;
use ccswap_core::usage::now_secs;
use ccswap_core::usage_store::UsageEntry;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use serde_json::{Value, json};

use super::app::{App, AppConfig};
use super::engine::{EngineControl, EngineFactory, EngineReport};
use super::theme::ThemeName;
use super::view;

pub fn iso_in(seconds: f64) -> String {
    let at = chrono::Utc::now() + chrono::Duration::milliseconds((seconds * 1000.0) as i64);
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `None` omits that window (annual plans have no 7d window).
/// Usage measured at `fetched_at` (upstream `make_usage_at`).
pub fn entry_at(fetched_at: f64, pct: f64) -> UsageEntry {
    UsageEntry {
        last_good: Some(json!({"five_hour": {"pct": pct, "resets_at": iso_in(7200.0)}})),
        fetched_at: Some(fetched_at),
        age_s: Some(5.0),
        ..Default::default()
    }
}

pub fn entry(pct5: Option<f64>, pct7: Option<f64>) -> UsageEntry {
    let mut last_good = serde_json::Map::new();
    if let Some(p) = pct5 {
        last_good.insert(
            "five_hour".into(),
            json!({"pct": p, "resets_at": iso_in(7200.0)}),
        );
    }
    if let Some(p) = pct7 {
        last_good.insert(
            "seven_day".into(),
            json!({"pct": p, "resets_at": iso_in(86400.0 * 3.0)}),
        );
    }
    UsageEntry {
        last_good: Some(Value::Object(last_good)),
        fetched_at: Some(now_secs() - 5.0),
        age_s: Some(5.0),
        ..Default::default()
    }
}

pub fn make_account(number: &str, active: bool, usage: UsageEntry) -> AccountSnapshot {
    AccountSnapshot {
        number: number.into(),
        email: format!("user{number}@example.com"),
        org_name: String::new(),
        org_uuid: String::new(),
        is_active: active,
        kind: AccountKind::OAuth,
        switchable: true,
        usage,
        alias: String::new(),
        disabled: false,
    }
}

pub fn account(number: &str, active: bool) -> AccountSnapshot {
    make_account(number, active, entry(Some(25.0), Some(10.0)))
}

pub fn plain_lines(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    SwitchTo(String),
    Switch(Option<&'static str>),
    Remove(String, bool),
    SetDisabled(String, bool),
    Add(Option<i64>, bool),
    AddToken(String, Option<String>, Option<i64>, bool),
}

/// Structured-surface stand-in: switches and removals change what the
/// next snapshot shows.
pub struct FakeActions {
    pub accounts: Mutex<Vec<AccountSnapshot>>,
    pub active: Mutex<Option<String>>,
    pub calls: Mutex<Vec<Call>>,
    pub fetch_sets: Mutex<Vec<Option<HashSet<String>>>>,
    pub poll_inputs: Mutex<Option<(f64, Vec<String>)>>,
    pub lanes: Mutex<Option<Arc<Lanes>>>,
    pub backup_dir: PathBuf,
}

/// Blocks a snapshot pass until opened.
pub struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    pub fn new(open: bool) -> Self {
        Gate {
            open: Mutex::new(open),
            cv: Condvar::new(),
        }
    }

    pub fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }

    fn pass(&self) {
        let open = self.open.lock().unwrap();
        let _open = self.cv.wait_while(open, |o| !*o).unwrap();
    }
}

/// Upstream `BlockingSnapshotSwitcher`: the network lane (`fetch=None`)
/// and the store lane each answer with their own accounts, behind a gate.
pub struct Lanes {
    pub normal: Vec<AccountSnapshot>,
    pub store: Vec<AccountSnapshot>,
    pub normal_gate: Gate,
    pub store_gate: Gate,
}

impl FakeActions {
    pub fn new(accounts: Vec<AccountSnapshot>, backup_dir: PathBuf) -> Arc<Self> {
        let active = accounts
            .iter()
            .find(|a| a.is_active)
            .map(|a| a.number.clone());
        Arc::new(FakeActions {
            accounts: Mutex::new(accounts),
            active: Mutex::new(active),
            calls: Mutex::new(Vec::new()),
            fetch_sets: Mutex::new(Vec::new()),
            poll_inputs: Mutex::new(None),
            lanes: Mutex::new(None),
            backup_dir,
        })
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}

impl Actions for FakeActions {
    fn backup_dir(&self) -> PathBuf {
        self.backup_dir.clone()
    }

    fn accounts_snapshot(&self, fetch: Option<&HashSet<String>>) -> Result<AccountsSnapshot> {
        self.fetch_sets.lock().unwrap().push(fetch.cloned());
        let lanes = self.lanes.lock().unwrap().clone();
        if let Some(lanes) = lanes {
            let (gate, accounts) = match fetch {
                None => (&lanes.normal_gate, &lanes.normal),
                Some(_) => (&lanes.store_gate, &lanes.store),
            };
            gate.pass();
            return Ok(AccountsSnapshot {
                active_number: Some("1".into()),
                accounts: accounts.clone(),
                taken_at: now_secs(),
            });
        }
        Ok(AccountsSnapshot {
            active_number: self.active.lock().unwrap().clone(),
            accounts: self.accounts.lock().unwrap().clone(),
            taken_at: now_secs(),
        })
    }

    fn current_account_number(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }

    fn switch_to(&self, identifier: &str, _: bool, _: &mut dyn Console) -> Result<Option<Value>> {
        self.record(Call::SwitchTo(identifier.into()));
        let old = self.active.lock().unwrap().replace(identifier.into());
        for acc in self.accounts.lock().unwrap().iter_mut() {
            acc.is_active = acc.number == identifier;
        }
        Ok(Some(json!({
            "switched": true,
            "from": {"number": old.and_then(|o| o.parse::<i64>().ok()), "email": ""},
            "to": {"number": identifier.parse::<i64>().ok(), "email": format!("user{identifier}@example.com")},
            "reason": "requested",
        })))
    }

    fn switch(
        &self,
        strategy: Option<&SwitchStrategy>,
        _: &mut dyn Console,
    ) -> Result<Option<Value>> {
        self.record(Call::Switch(strategy.map(|s| s.strategy.label())));
        Ok(Some(
            json!({"switched": false, "from": null, "to": null, "reason": "no-better-target"}),
        ))
    }

    fn remove_account(
        &self,
        identifier: &str,
        assume_yes: bool,
        c: &mut dyn Console,
    ) -> Result<()> {
        self.record(Call::Remove(identifier.into(), assume_yes));
        self.accounts
            .lock()
            .unwrap()
            .retain(|a| a.number != identifier);
        c.line(&format!("Removed account {identifier}"));
        Ok(())
    }

    fn set_account_disabled(
        &self,
        identifier: &str,
        disabled: bool,
        c: &mut dyn Console,
    ) -> Result<()> {
        self.record(Call::SetDisabled(identifier.into(), disabled));
        for acc in self.accounts.lock().unwrap().iter_mut() {
            if acc.number == identifier {
                acc.disabled = disabled;
            }
        }
        let verb = if disabled { "Disabled" } else { "Enabled" };
        c.line(&format!("{verb} Account-{identifier}"));
        Ok(())
    }

    fn add_account(&self, slot: Option<i64>, assume_yes: bool, c: &mut dyn Console) -> Result<()> {
        self.record(Call::Add(slot, assume_yes));
        c.line("Added Account 9: fresh@example.com");
        Ok(())
    }

    fn add_account_from_token(
        &self,
        token: &str,
        email: Option<&str>,
        slot: Option<i64>,
        assume_yes: bool,
        c: &mut dyn Console,
    ) -> Result<()> {
        self.record(Call::AddToken(
            token.into(),
            email.map(str::to_string),
            slot,
            assume_yes,
        ));
        if token == "bad" {
            return Err(Error::new(
                ErrorKind::ValidationError,
                "Token cannot be empty",
            ));
        }
        c.line(&format!("Added Account {}", slot.unwrap_or(9)));
        Ok(())
    }

    fn set_poll_policy_inputs(&self, threshold: f64, models: Vec<String>) {
        *self.poll_inputs.lock().unwrap() = Some((threshold, models));
    }

    fn clear_poll_policy_inputs(&self) {
        *self.poll_inputs.lock().unwrap() = None;
    }
}

/// An app over a fake, started and settled like upstream's `settle(pilot)`.
pub struct Harness {
    pub fake: Arc<FakeActions>,
    pub engines: Arc<FakeEngines>,
    pub app: App,
    _dir: tempfile::TempDir,
}

impl Harness {
    pub fn new(accounts: Vec<AccountSnapshot>) -> Self {
        let config = AppConfig {
            threshold: Some(90.0),
            theme: ThemeName::Dark,
            detected_light: None,
        };
        Self::with_config(accounts, config)
    }

    /// `settings` is written to the store's settings.json first.
    pub fn with_settings(accounts: Vec<AccountSnapshot>, settings: Option<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.json"), settings.to_string()).unwrap();
        }
        let fake = FakeActions::new(accounts, dir.path().to_path_buf());
        let config = AppConfig::load(&Settings::in_dir(dir.path()));
        Self::start(fake, config, dir)
    }

    pub fn with_config(accounts: Vec<AccountSnapshot>, config: AppConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeActions::new(accounts, dir.path().to_path_buf());
        Self::start(fake, config, dir)
    }

    fn start(fake: Arc<FakeActions>, config: AppConfig, dir: tempfile::TempDir) -> Self {
        let engines = Arc::new(FakeEngines::default());
        let mut app = App::new(fake.clone(), config).with_engine_factory(engines.factory());
        app.tick();
        app.settle();
        Harness {
            fake,
            engines,
            app,
            _dir: dir,
        }
    }

    /// Starts the app over [`Lanes`] without waiting for the first pass.
    pub fn with_lanes(lanes: Lanes) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeActions::new(Vec::new(), dir.path().to_path_buf());
        *fake.lanes.lock().unwrap() = Some(Arc::new(lanes));
        let engines = Arc::new(FakeEngines::default());
        let config = AppConfig {
            threshold: Some(90.0),
            theme: ThemeName::Dark,
            detected_light: None,
        };
        let mut app = App::new(fake.clone(), config).with_engine_factory(engines.factory());
        app.tick();
        Harness {
            fake,
            engines,
            app,
            _dir: dir,
        }
    }

    pub fn fetch_sets(&self) -> Vec<Option<HashSet<String>>> {
        self.fake.fetch_sets.lock().unwrap().clone()
    }

    /// Spins until the fake has seen `n` snapshot passes start.
    pub fn wait_for_passes(&self, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.fetch_sets().len() < n {
            assert!(
                std::time::Instant::now() < deadline,
                "pass {n} never started"
            );
            std::thread::yield_now();
        }
    }

    pub fn wait_until(&mut self, done: impl Fn(&App) -> bool) {
        self.app.wait_until(done);
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.fake.backup_dir.clone()
    }

    pub fn press(&mut self, keys: &[&str]) {
        for key in keys {
            self.app.handle_key(key_event(key));
        }
    }

    pub fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.app
                .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    pub fn settle(&mut self) {
        self.app.settle();
    }

    pub fn calls(&self) -> Vec<Call> {
        self.fake.calls()
    }

    /// The screen as text, one row per line, trailing blanks trimmed.
    pub fn render(&self, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| view::draw(f, &self.app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Highlights the dashboard menu entry with this id and presses Enter.
    pub fn menu_select(&mut self, action_id: &str) {
        let index = self
            .app
            .dashboard()
            .entries()
            .iter()
            .position(|e| e.action_id == action_id)
            .unwrap_or_else(|| panic!("no menu entry {action_id}"));
        let current = self.app.dashboard().index;
        let key = if index > current { "down" } else { "up" };
        for _ in 0..index.abs_diff(current) {
            self.press(&[key]);
        }
        self.press(&["enter"]);
    }

    pub fn menu_ids(&self) -> Vec<String> {
        self.app
            .dashboard()
            .entries()
            .iter()
            .map(|e| e.action_id.clone())
            .collect()
    }

    pub fn menu_labels(&self) -> Vec<String> {
        self.app
            .dashboard()
            .entries()
            .iter()
            .map(|e| e.label.clone())
            .collect()
    }
}

pub fn key_event(name: &str) -> KeyEvent {
    let code = match name {
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "ctrl+t" => return KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        other => panic!("unknown key {other}"),
    };
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// One stand-in engine (upstream `_FakeEngine`): reports one `no-switch`
/// event, then idles until stopped.
#[derive(Debug)]
pub struct FakeEngine {
    pub settings: AutoSwitchSettings,
    pub dry_run: bool,
    pub stopped: bool,
    pub applied_thresholds: Vec<f64>,
    pub wakes: usize,
}

#[derive(Default)]
pub struct FakeEngines {
    pub instances: Mutex<Vec<FakeEngine>>,
}

struct FakeControl {
    engines: Arc<FakeEngines>,
    index: usize,
}

impl FakeControl {
    fn with(&self, f: impl FnOnce(&mut FakeEngine)) {
        f(&mut self.engines.instances.lock().unwrap()[self.index]);
    }
}

impl EngineControl for FakeControl {
    fn stop(&self) {
        self.with(|e| e.stopped = true);
    }

    fn wake(&self) {
        self.with(|e| e.wakes += 1);
    }

    fn apply_threshold(&self, threshold: f64) {
        self.with(|e| {
            e.settings.threshold = threshold;
            e.applied_thresholds.push(threshold);
        });
    }
}

impl FakeEngines {
    pub fn factory(self: &Arc<Self>) -> EngineFactory {
        let engines = self.clone();
        Arc::new(move |start| {
            let mut instances = engines.instances.lock().unwrap();
            instances.push(FakeEngine {
                settings: start.settings,
                dry_run: start.dry_run,
                stopped: false,
                applied_thresholds: Vec::new(),
                wakes: 0,
            });
            let index = instances.len() - 1;
            let sink = start.sink;
            std::thread::spawn(move || {
                sink(EngineReport::Event(AutoSwitchEvent {
                    ts: "2026-01-01T00:00:00Z".into(),
                    kind: EventKind::NoSwitch {
                        reason: "cooldown",
                        detail: String::new(),
                    },
                }))
            });
            Box::new(FakeControl {
                engines: engines.clone(),
                index,
            })
        })
    }

    pub fn with<T>(&self, f: impl FnOnce(&[FakeEngine]) -> T) -> T {
        f(&self.instances.lock().unwrap())
    }
}
