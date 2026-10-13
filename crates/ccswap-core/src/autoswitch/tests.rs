//! Tick sequences on an in-memory host with a stepped clock (upstream
//! `tests/test_autoswitch.py`, `EngineHarness` + `FakeClock`). Test names
//! keep upstream's `Class::test` names.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::anthropic_client::RefreshOutcome;
use crate::store::SlotIdentity;
use crate::usage::Sentinel;
use crate::usage_store::UsageEntry;

const START: f64 = 1_000_000.0;

type Provider = Box<dyn FnMut(&HashSet<String>) -> Vec<(String, UsageEntry)>>;
/// `(slot, snapshot)` to the gate's outcome.
type RefreshFn = Box<dyn Fn(&str, &str) -> RefreshOutcome>;

struct FakeHost {
    root: tempfile::TempDir,
    slots: RefCell<Vec<(String, String, AccountKind)>>,
    creds: RefCell<HashMap<String, String>>,
    current: RefCell<Option<String>>,
    live_login: Cell<bool>,
    provider: RefCell<Option<Provider>>,
    fetch_sets: RefCell<Vec<HashSet<String>>>,
    switch_override: RefCell<Option<SwitchResult>>,
    live_sessions: RefCell<HashSet<String>>,
    refresh: RefCell<Option<RefreshOutcome>>,
    refresh_fn: RefCell<Option<RefreshFn>>,
    refresh_calls: Cell<usize>,
    identities: RefCell<HashMap<String, SlotIdentity>>,
    backfills: RefCell<Vec<(String, String)>>,
}

impl FakeHost {
    fn new() -> Self {
        FakeHost {
            root: tempfile::tempdir().unwrap(),
            slots: RefCell::new(Vec::new()),
            creds: RefCell::new(HashMap::new()),
            current: RefCell::new(None),
            live_login: Cell::new(false),
            provider: RefCell::new(None),
            fetch_sets: RefCell::new(Vec::new()),
            switch_override: RefCell::new(None),
            live_sessions: RefCell::new(HashSet::new()),
            refresh: RefCell::new(None),
            refresh_fn: RefCell::new(None),
            refresh_calls: Cell::new(0),
            identities: RefCell::new(HashMap::new()),
            backfills: RefCell::new(Vec::new()),
        }
    }

    fn seed(&self, num: u32, email: &str) {
        self.seed_expiring(num, email, None);
    }

    fn seed_expiring(&self, num: u32, email: &str, expires_at: Option<i64>) {
        let mut oauth =
            json!({"accessToken": format!("sk-{num}"), "refreshToken": format!("rt-{num}")});
        if let Some(e) = expires_at {
            oauth["expiresAt"] = json!(e);
        }
        self.creds
            .borrow_mut()
            .insert(num.to_string(), json!({"claudeAiOauth": oauth}).to_string());
        self.slots
            .borrow_mut()
            .push((num.to_string(), email.to_string(), AccountKind::OAuth));
        self.identities.borrow_mut().insert(
            num.to_string(),
            SlotIdentity {
                email: email.to_string(),
                organization_uuid: String::new(),
                uuid: format!("uuid-{num}"),
            },
        );
    }

    fn mark_api_key(&self, num: u32) {
        for slot in self.slots.borrow_mut().iter_mut() {
            if slot.0 == num.to_string() {
                slot.2 = AccountKind::ApiKey;
            }
        }
    }

    fn make_live(&self, email: &str) {
        self.live_login.set(true);
        let slot = self
            .slots
            .borrow()
            .iter()
            .find(|s| s.1 == email)
            .map(|s| s.0.clone());
        *self.current.borrow_mut() = slot;
    }

    fn active_number(&self) -> Option<String> {
        self.current.borrow().clone()
    }

    fn serve(&self, provider: Provider) {
        *self.provider.borrow_mut() = Some(provider);
    }

    fn state(&self) -> Value {
        let path = self.root.path().join(STATE_FILENAME);
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap(),
            Err(_) => json!({}),
        }
    }
}

impl EngineHost for FakeHost {
    fn backup_root(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    fn current_account_number(&self) -> Option<String> {
        self.current.borrow().clone()
    }

    fn has_live_login(&self) -> bool {
        self.live_login.get()
    }

    fn account_email(&self, num: &str) -> String {
        self.slots
            .borrow()
            .iter()
            .find(|s| s.0 == num)
            .map(|s| s.1.clone())
            .unwrap_or_default()
    }

    fn account_kind(&self, num: &str) -> AccountKind {
        self.slots
            .borrow()
            .iter()
            .find(|s| s.0 == num)
            .map_or(AccountKind::OAuth, |s| s.2)
    }

    fn switchable_account_numbers(&self) -> Vec<String> {
        self.slots.borrow().iter().map(|s| s.0.clone()).collect()
    }

    fn usage_entries(
        &self,
        fetch: &HashSet<String>,
        _scheduled: bool,
    ) -> Vec<(String, UsageEntry)> {
        self.fetch_sets.borrow_mut().push(fetch.clone());
        let mut provider = self.provider.borrow_mut();
        match provider.as_mut() {
            Some(p) => p(fetch),
            None => Vec::new(),
        }
    }

    fn read_account_credentials(&self, num: &str, _email: &str) -> String {
        self.creds.borrow().get(num).cloned().unwrap_or_default()
    }

    fn live_session_pids_for(&self, num: &str, _email: &str) -> Vec<i32> {
        if self.live_sessions.borrow().contains(num) {
            vec![4242]
        } else {
            Vec::new()
        }
    }

    /// Like the real gate, a successful refresh persists the successor.
    fn consume_backup_grant(&self, num: &str, _email: &str, snapshot: &str) -> RefreshOutcome {
        self.refresh_calls.set(self.refresh_calls.get() + 1);
        let outcome = match self.refresh_fn.borrow().as_ref() {
            Some(f) => f(num, snapshot),
            None => self.refresh.borrow().clone().unwrap_or(RefreshOutcome {
                error: Some("consume-busy".into()),
                ..Default::default()
            }),
        };
        if outcome.error.is_none()
            && let Some(creds) = &outcome.credentials
        {
            self.creds.borrow_mut().insert(num.into(), creds.clone());
        }
        outcome
    }

    fn account_identity(&self, num: &str) -> SlotIdentity {
        self.identities
            .borrow()
            .get(num)
            .cloned()
            .unwrap_or_default()
    }

    fn backfill_account_uuid(&self, num: &str, uuid: &str) {
        self.backfills
            .borrow_mut()
            .push((num.to_string(), uuid.to_string()));
        if let Some(identity) = self.identities.borrow_mut().get_mut(num)
            && identity.uuid.is_empty()
        {
            identity.uuid = uuid.to_string();
        }
    }

    fn switch_to(&self, num: &str) -> crate::error::Result<SwitchResult> {
        if let Some(result) = self.switch_override.borrow().clone() {
            return Ok(result);
        }
        let from = self.current.borrow().clone();
        let r =
            |n: &str| json!({"number": n.parse::<i64>().unwrap(), "email": self.account_email(n)});
        if from.as_deref() == Some(num) {
            return Ok(SwitchResult {
                switched: false,
                reason: "already-active".into(),
                from: Some(r(num)),
                to: Some(r(num)),
                warnings: Vec::new(),
            });
        }
        *self.current.borrow_mut() = Some(num.to_string());
        Ok(SwitchResult {
            switched: true,
            reason: "switched".into(),
            from: from.as_deref().map(r),
            to: Some(r(num)),
            warnings: Vec::new(),
        })
    }

    fn set_poll_policy_inputs(&self, _threshold: f64, _models: &[String]) {}
}

/// `FakeClock`: starts at 1_000_000.0 and only moves when advanced.
#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    fn new() -> Self {
        FakeClock(Arc::new(AtomicU64::new(START.to_bits())))
    }
    fn now(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }
    fn advance(&self, seconds: f64) {
        self.0
            .store((self.now() + seconds).to_bits(), Ordering::SeqCst);
    }
    fn clock(&self) -> Clock {
        let inner = self.0.clone();
        Arc::new(move || f64::from_bits(inner.load(Ordering::SeqCst)))
    }
}

type Events = Rc<RefCell<Vec<AutoSwitchEvent>>>;

struct Harness {
    host: FakeHost,
    clock: FakeClock,
    events: Events,
}

impl Harness {
    fn bare() -> Self {
        Harness {
            host: FakeHost::new(),
            clock: FakeClock::new(),
            events: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Slots 1..3 with account 1 live (the upstream `harness` fixture).
    fn three() -> Self {
        let h = Self::bare();
        h.host.seed(1, "a@example.com");
        h.host.seed(2, "b@example.com");
        h.host.seed(3, "c@example.com");
        h.host.make_live("a@example.com");
        h
    }

    fn two() -> Self {
        let h = Self::bare();
        h.host.seed(1, "a@example.com");
        h.host.seed(2, "b@example.com");
        h.host.make_live("a@example.com");
        h
    }

    fn engine(&self, settings: AutoSwitchSettings) -> AutoSwitchEngine<'_> {
        let events = self.events.clone();
        AutoSwitchEngine::new(
            &self.host,
            settings,
            Box::new(move |e| events.borrow_mut().push(e)),
        )
        .clock(self.clock.clock())
        .rng(Box::new(|| 0.5))
    }

    fn default_engine(&self) -> AutoSwitchEngine<'_> {
        self.engine(AutoSwitchSettings::default())
    }

    /// Every collection serves these values (upstream `tick_with_usage`).
    fn serve(&self, usage: &[(&str, Option<Value>)]) {
        self.serve_values(
            usage
                .iter()
                .map(|(n, v)| {
                    (
                        n.to_string(),
                        v.clone().map(Val::Dict).unwrap_or(Val::Unknown),
                    )
                })
                .collect(),
        );
    }

    fn serve_values(&self, usage: Vec<(String, Val)>) {
        let clock = self.clock.clone();
        self.host.serve(Box::new(move |_| {
            usage
                .iter()
                .map(|(n, v)| (n.clone(), entry_for(v, clock.now())))
                .collect()
        }));
    }

    fn serve_entries(&self, entries: Vec<(&str, UsageEntry)>) {
        let entries: Vec<(String, UsageEntry)> = entries
            .into_iter()
            .map(|(n, e)| (n.to_string(), e))
            .collect();
        self.host.serve(Box::new(move |_| entries.clone()));
    }

    fn tick(&self, engine: &mut AutoSwitchEngine, usage: &[(&str, Option<Value>)]) -> TickOutcome {
        self.serve(usage);
        engine.tick()
    }

    fn reasons(&self) -> Vec<&'static str> {
        self.events
            .borrow()
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::NoSwitch { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    fn details(&self) -> Vec<String> {
        self.events
            .borrow()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NoSwitch { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    fn switch(&self) -> Option<(Trigger, Value, Value, bool)> {
        self.events.borrow().iter().find_map(|e| match &e.kind {
            EventKind::Switch {
                trigger,
                from,
                to,
                dry_run,
                ..
            } => Some((
                *trigger,
                from.clone().unwrap_or(Value::Null),
                to.clone().unwrap_or(Value::Null),
                *dry_run,
            )),
            _ => None,
        })
    }

    fn has(&self, name: &str) -> bool {
        self.events.borrow().iter().any(|e| e.name() == name)
    }

    fn count(&self, name: &str) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|e| e.name() == name)
            .count()
    }

    fn exhausted_at(&self) -> Option<Option<String>> {
        self.events.borrow().iter().find_map(|e| match &e.kind {
            EventKind::AllExhausted { earliest_reset_at } => Some(earliest_reset_at.clone()),
            _ => None,
        })
    }

    fn active(&self) -> Option<String> {
        self.host.active_number()
    }
}

#[derive(Clone)]
enum Val {
    Dict(Value),
    Sentinel(Sentinel),
    Unknown,
}

fn entry_for(value: &Val, now: f64) -> UsageEntry {
    match value {
        Val::Dict(v) => UsageEntry {
            last_good: Some(v.clone()),
            fetched_at: Some(now),
            age_s: Some(0.0),
            ..Default::default()
        },
        Val::Sentinel(s) => UsageEntry {
            sentinel: Some(*s),
            ..Default::default()
        },
        Val::Unknown => UsageEntry::default(),
    }
}

fn usage(pct: f64) -> Option<Value> {
    Some(json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}}))
}

fn usage_reset(pct: f64, resets_at: &str) -> Option<Value> {
    Some(json!({"five_hour": {"pct": pct, "resets_at": resets_at}, "seven_day": {"pct": 0.0}}))
}

fn model_usage(five_h: f64, fable: f64) -> Option<Value> {
    Some(json!({
        "five_hour": {"pct": five_h},
        "seven_day": {"pct": 0.0},
        "scoped": [{"name": "Fable", "pct": fable}],
    }))
}

fn usage7(pct5: f64, pct7: f64, reset7: Option<&str>) -> Option<Value> {
    let mut seven = json!({"pct": pct7});
    if let Some(r) = reset7 {
        seven["resets_at"] = json!(r);
    }
    Some(json!({"five_hour": {"pct": pct5}, "seven_day": seven}))
}

fn with(f: impl FnOnce(&mut AutoSwitchSettings)) -> AutoSwitchSettings {
    let mut s = AutoSwitchSettings::default();
    f(&mut s);
    s
}

fn s(n: &str) -> Option<String> {
    Some(n.to_string())
}

// --- TestDecisionTable --------------------------------------------------------

#[test]
fn decision_table_test_below_threshold_is_no_action() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(10.0))],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn decision_table_test_over_threshold_switches_to_max_headroom() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(40.0)), ("3", usage(20.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
    let (trigger, _, to, _) = h.switch().unwrap();
    assert_eq!(trigger, Trigger::Proactive);
    assert_eq!(to, json!({"number": 3, "email": "c@example.com"}));
    assert_eq!(h.host.state()["lastSwitchTo"], "3");
}

#[test]
fn decision_table_test_no_active_account() {
    let h = Harness::bare();
    let mut e = h.default_engine();
    assert_eq!(e.tick(), TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["no-active-account"]);
}

#[test]
fn decision_table_test_hysteresis_margin_blocks_marginal_candidates() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(86.0)), ("3", usage(88.0))],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
    assert!(!h.has("all-exhausted"));
    assert_eq!(h.reasons(), ["no-qualifying-candidate"]);
    assert_eq!(e.sleep_until_ts(), None);
    assert!(e.next_delay(out) <= 1.1 * e.settings().interval_seconds);
}

#[test]
fn decision_table_test_issue_115_strictly_better_candidate_switches() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            (
                "1",
                Some(json!({"five_hour": {"pct": 99.0}, "seven_day": {"pct": 24.0}})),
            ),
            (
                "2",
                Some(json!({"five_hour": {"pct": 3.0}, "seven_day": {"pct": 89.0}})),
            ),
            (
                "3",
                Some(json!({"five_hour": {"pct": 95.0}, "seven_day": {"pct": 10.0}})),
            ),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.switch().unwrap().0, Trigger::Proactive);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_proactive_never_lands_at_or_over_threshold() {
    let h = Harness::two();
    let mut e = h.engine(with(|s| {
        s.threshold = 80.0;
        s.hysteresis_pct = 5.0;
    }));
    let out = h.tick(&mut e, &[("1", usage(90.0)), ("2", usage(85.0))]);
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
    assert_eq!(h.reasons(), ["no-qualifying-candidate"]);
}

#[test]
fn decision_table_test_stable_landing_does_not_switch_back() {
    let h = Harness::two();
    let mut e = h.engine(with(|s| s.cooldown_seconds = 0.0));
    let u = [
        (
            "1",
            Some(json!({"five_hour": {"pct": 99.0}, "seven_day": {"pct": 24.0}})),
        ),
        (
            "2",
            Some(json!({"five_hour": {"pct": 3.0}, "seven_day": {"pct": 89.0}})),
        ),
    ];
    assert_eq!(h.tick(&mut e, &u), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    h.clock.advance(60.0);
    assert_eq!(h.tick(&mut e, &u), TickOutcome::NoAction);
    assert_eq!(h.active(), s("2"));
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn decision_table_test_mixed_unknown_and_exhausted_is_not_all_exhausted() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", usage(95.0)),
            ("2", usage_reset(100.0, "2026-07-03T12:00:00Z")),
            ("3", None),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert!(!h.has("all-exhausted"));
    assert_eq!(h.reasons(), ["no-qualifying-candidate"]);
    assert_eq!(e.sleep_until_ts(), None);
    assert!(e.next_delay(out) <= 1.1 * e.settings().interval_seconds);
}

#[test]
fn decision_table_test_stale_beyond_trust_blocks_all_exhausted() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let now = h.clock.now();
    let reset = "2026-07-05T12:00:00Z";
    h.serve_entries(vec![
        ("1", entry_for(&Val::Dict(usage(95.0).unwrap()), now)),
        (
            "2",
            UsageEntry {
                last_good: usage_reset(100.0, reset),
                fetched_at: Some(now - 400.0),
                age_s: Some(400.0),
                consecutive_failures: 1,
                trust_extended: true,
                ..Default::default()
            },
        ),
        (
            "3",
            UsageEntry {
                last_good: usage(10.0),
                fetched_at: Some(now - 400.0),
                age_s: Some(400.0),
                ..Default::default()
            },
        ),
    ]);
    assert_eq!(e.tick(), TickOutcome::Blocked);
    assert!(!h.has("all-exhausted"));
    assert_eq!(h.reasons(), ["no-qualifying-candidate"]);
}

#[test]
fn decision_table_test_trusted_stale_exhausted_set_still_fires_all_exhausted() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let now = h.clock.now();
    let reset = "2026-07-05T12:00:00Z";
    let stale_exhausted = UsageEntry {
        last_good: usage_reset(100.0, reset),
        fetched_at: Some(now - 400.0),
        age_s: Some(400.0),
        consecutive_failures: 1,
        trust_extended: true,
        ..Default::default()
    };
    h.serve_entries(vec![
        ("1", entry_for(&Val::Dict(usage(95.0).unwrap()), now)),
        ("2", stale_exhausted.clone()),
        ("3", stale_exhausted),
    ]);
    assert_eq!(e.tick(), TickOutcome::Blocked);
    assert_eq!(h.exhausted_at(), Some(s(reset)));
}

fn set_last_switch_at(h: &Harness, at: f64) {
    StateFile::new(h.host.root.path().join(STATE_FILENAME))
        .mutate(|s| {
            s.insert("lastSwitchAt".into(), json!(at));
        })
        .unwrap();
}

#[test]
fn decision_table_test_cooldown_suppresses_proactive() {
    let h = Harness::three();
    let mut e = h.default_engine();
    set_last_switch_at(&h, h.clock.now() - 10.0);
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(10.0))],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["cooldown"]);
}

#[test]
fn decision_table_test_at_limit_bypasses_cooldown() {
    let h = Harness::three();
    let mut e = h.default_engine();
    set_last_switch_at(&h, h.clock.now() - 10.0);
    let out = h.tick(
        &mut e,
        &[("1", usage(100.0)), ("2", usage(10.0)), ("3", usage(50.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.switch().unwrap().0, Trigger::AtLimit);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_cooldown_expires() {
    let h = Harness::three();
    let mut e = h.default_engine();
    set_last_switch_at(&h, h.clock.now());
    h.clock.advance(400.0);
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(50.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
}

#[test]
fn decision_table_test_unknown_active_usage_waits_then_fails_over() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let u = [("1", None), ("2", usage(10.0)), ("3", usage(50.0))];
    assert_eq!(h.tick(&mut e, &u), TickOutcome::NoAction);
    assert_eq!(h.tick(&mut e, &u), TickOutcome::NoAction);
    assert_eq!(h.tick(&mut e, &u), TickOutcome::Switched);
    assert_eq!(h.switch().unwrap().0, Trigger::Failover);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_known_active_usage_resets_unhealthy_counter() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let unknown = [("1", None), ("2", usage(10.0)), ("3", usage(10.0))];
    let healthy = [("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(10.0))];
    h.tick(&mut e, &unknown);
    h.tick(&mut e, &unknown);
    h.tick(&mut e, &healthy);
    assert_eq!(h.tick(&mut e, &unknown), TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn decision_table_test_all_candidates_unknown_is_no_comparison() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", None), ("3", None)]);
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.reasons(), ["no-comparison"]);
}

#[test]
fn decision_table_test_tie_resolves_to_earliest_slot() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(30.0)), ("3", usage(30.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_candidate_not_better_than_active_is_skipped() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(91.0)), ("2", usage(95.0)), ("3", usage(99.0))],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn decision_table_test_at_limit_escapes_hysteresis_bar() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(100.0)), ("2", usage(85.0)), ("3", usage(97.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.switch().unwrap().0, Trigger::AtLimit);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_at_limit_never_targets_another_at_limit_account() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", usage(100.0)),
            ("2", usage(100.0)),
            ("3", usage(100.0)),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn decision_table_test_failover_ignores_hysteresis_bar() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let u = [("1", None), ("2", usage(85.0)), ("3", usage(100.0))];
    h.tick(&mut e, &u);
    h.tick(&mut e, &u);
    assert_eq!(h.tick(&mut e, &u), TickOutcome::Switched);
    assert_eq!(h.switch().unwrap().0, Trigger::Failover);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn decision_table_test_unmanaged_live_login_is_never_touched() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed(2, "b@example.com");
    h.host.make_live("stranger@example.com");
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["unmanaged-active-account"]);
    assert_eq!(h.active(), None);
}

#[test]
fn decision_table_test_all_exhausted_carries_earliest_reset() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", usage_reset(100.0, "2026-07-03T12:00:00Z")),
            ("2", usage_reset(100.0, "2026-07-03T10:30:00Z")),
            ("3", usage_reset(100.0, "2026-07-03T11:00:00Z")),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.exhausted_at(), Some(s("2026-07-03T10:30:00Z")));
    assert!(e.sleep_until_ts().is_some());
}

#[test]
fn decision_table_test_all_exhausted_ignores_non_future_reset() {
    for offset in [-60.0, 0.0] {
        let h = Harness::three();
        let mut e = h.default_engine();
        let reset = iso_utc(h.clock.now() + offset);
        let out = h.tick(
            &mut e,
            &[
                ("1", usage_reset(100.0, &reset)),
                ("2", usage_reset(100.0, &reset)),
                ("3", usage_reset(100.0, &reset)),
            ],
        );
        assert_eq!(out, TickOutcome::Blocked);
        assert_eq!(h.exhausted_at(), Some(None));
        assert_eq!(e.sleep_until_ts(), None);
        assert_eq!(e.next_delay(out), NO_RESET_FALLBACK_S);
    }
}

// --- TestApiKeyAccounts -------------------------------------------------------

#[test]
fn api_key_accounts_test_api_key_candidate_excluded_by_default() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed(2, "key@token.local");
    h.host.make_live("a@example.com");
    h.host.mark_api_key(2);
    let mut e = h.default_engine();
    h.serve_values(vec![
        ("1".into(), Val::Dict(usage(95.0).unwrap())),
        ("2".into(), Val::Sentinel(Sentinel::ApiKey)),
    ]);
    assert_eq!(e.tick(), TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
}

fn api_key_fleet() -> Harness {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed(2, "key@token.local");
    h.host.seed(3, "c@example.com");
    h.host.make_live("a@example.com");
    h.host.mark_api_key(2);
    h
}

#[test]
fn api_key_accounts_test_api_key_is_last_resort_when_included() {
    let h = api_key_fleet();
    let mut e = h.engine(with(|s| s.include_api_key_accounts = true));
    h.serve_values(vec![
        ("1".into(), Val::Dict(usage(95.0).unwrap())),
        ("2".into(), Val::Sentinel(Sentinel::ApiKey)),
        ("3".into(), Val::Dict(usage(10.0).unwrap())),
    ]);
    assert_eq!(e.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn api_key_accounts_test_api_key_used_when_oauth_exhausted() {
    let h = api_key_fleet();
    let mut e = h.engine(with(|s| s.include_api_key_accounts = true));
    h.serve_values(vec![
        ("1".into(), Val::Dict(usage(100.0).unwrap())),
        ("2".into(), Val::Sentinel(Sentinel::ApiKey)),
        ("3".into(), Val::Dict(usage(100.0).unwrap())),
    ]);
    assert_eq!(e.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn api_key_accounts_test_active_api_key_idles_engine() {
    let h = Harness::bare();
    h.host.seed(1, "key@token.local");
    h.host.seed(2, "b@example.com");
    h.host.make_live("key@token.local");
    h.host.mark_api_key(1);
    let mut e = h.default_engine();
    h.serve_values(vec![
        ("1".into(), Val::Sentinel(Sentinel::ApiKey)),
        ("2".into(), Val::Dict(usage(10.0).unwrap())),
    ]);
    assert_eq!(e.tick(), TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["active-api-key"]);
}

// --- TestFreshening (the parts that need no consume gate) ---------------------

#[test]
fn freshening_test_fresh_target_is_not_refreshed() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(
        2,
        "b@example.com",
        Some((START * 1000.0) as i64 + 3_600_000),
    );
    h.host.make_live("a@example.com");
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]),
        TickOutcome::Switched
    );
    assert_eq!(h.host.refresh_calls.get(), 0);
}

#[test]
fn freshening_test_invalid_grant_quarantines_and_tries_next() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.seed(3, "c@example.com");
    h.host.make_live("a@example.com");
    *h.host.refresh.borrow_mut() = Some(RefreshOutcome {
        error: Some("invalid_grant".into()),
        ..Default::default()
    });
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(20.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
    let q = h
        .events
        .borrow()
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Quarantined { number, reason, .. } => Some((number.clone(), reason.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(q, ("2".into(), "invalid_grant".into()));
    assert!(h.host.state()["quarantine"].get("2").is_some());
}

#[test]
fn freshening_test_transient_failure_skips_without_quarantine() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.make_live("a@example.com");
    *h.host.refresh.borrow_mut() = Some(RefreshOutcome {
        error: Some("transient".into()),
        ..Default::default()
    });
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Error);
    assert_eq!(h.active(), s("1"));
    assert!(h.host.state().get("quarantine").is_none());
    assert!(h.has("error"));
}

#[test]
fn freshening_test_live_session_target_is_skipped_even_with_fresh_token() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(
        2,
        "b@example.com",
        Some((START * 1000.0) as i64 + 3_600_000),
    );
    h.host.make_live("a@example.com");
    h.host.live_sessions.borrow_mut().insert("2".into());
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.host.refresh_calls.get(), 0);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn freshening_systemic_refusal_names_its_cause() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.make_live("a@example.com");
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Error);
    let message = h.events.borrow().iter().find_map(|e| match &e.kind {
        EventKind::Error { message, .. } => Some(message.clone()),
        _ => None,
    });
    assert_eq!(
        message.as_deref(),
        Some("could not freshen: another cswap surface holds the slot — retries next pass")
    );
}

// --- TestQuarantineLifecycle --------------------------------------------------

fn quarantine_two(h: &Harness) {
    let mut e = h.default_engine();
    e.quarantine("2", "b@example.com", "invalid_grant").unwrap();
    h.events.borrow_mut().clear();
}

#[test]
fn quarantine_lifecycle_test_quarantine_persists_across_engine_instances() {
    let h = Harness::three();
    quarantine_two(&h);
    let mut fresh = h.default_engine();
    let out = h.tick(
        &mut fresh,
        &[("1", usage(95.0)), ("2", usage(0.0)), ("3", usage(50.0))],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn quarantine_lifecycle_test_replaced_credentials_lift_quarantine() {
    let h = Harness::three();
    quarantine_two(&h);
    h.host.creds.borrow_mut().insert(
        "2".into(),
        json!({"claudeAiOauth": {"accessToken": "sk-2b", "refreshToken": "rt-2b"}}).to_string(),
    );
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(0.0)), ("3", usage(50.0))],
    );
    assert!(h.has("account-unquarantined"));
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert!(h.host.state()["quarantine"].get("2").is_none());
}

#[test]
fn quarantine_lifecycle_test_state_lock_preserves_concurrent_writes() {
    let h = Harness::three();
    let state = StateFile::new(h.host.root.path().join(STATE_FILENAME));
    state
        .mutate(|s| {
            s.insert(
                "quarantine".into(),
                json!({"3": {"email": "c@example.com", "reason": "invalid_grant",
                              "at": "x", "refreshTokenFingerprint": null}}),
            );
        })
        .unwrap();
    state
        .mutate(|s| {
            s.insert("lastSwitchAt".into(), json!(123.0));
        })
        .unwrap();
    let written = h.host.state();
    assert_eq!(written["lastSwitchAt"], 123.0);
    assert!(written["quarantine"].get("3").is_some());
}

// --- TestDryRunAndNoOp --------------------------------------------------------

#[test]
fn dry_run_and_noop_test_dry_run_mutates_nothing() {
    let h = Harness::two();
    let mut e = h.default_engine().dry_run(true);
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Switched);
    assert!(h.switch().unwrap().3);
    assert_eq!(h.active(), s("1"));
    assert_eq!(h.host.state(), json!({}));
}

#[test]
fn dry_run_and_noop_test_dry_run_never_freshens_or_quarantines() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.make_live("a@example.com");
    let mut e = h.default_engine().dry_run(true);
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.host.refresh_calls.get(), 0);
    assert_eq!(h.host.state(), json!({}));
}

#[test]
fn dry_run_and_noop_test_dry_run_does_not_release_quarantines() {
    let h = Harness::two();
    quarantine_two(&h);
    h.host.creds.borrow_mut().insert(
        "2".into(),
        json!({"claudeAiOauth": {"accessToken": "n", "refreshToken": "n"}}).to_string(),
    );
    let before = h.host.state();
    let mut e = h.default_engine().dry_run(true);
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert!(!h.has("account-unquarantined"));
    assert_eq!(h.host.state(), before);
    assert_eq!(out, TickOutcome::Blocked);
}

#[test]
fn dry_run_and_noop_test_already_active_result_is_noop() {
    let h = Harness::three();
    *h.host.switch_override.borrow_mut() = Some(SwitchResult {
        switched: false,
        reason: "already-active".into(),
        from: None,
        to: None,
        warnings: Vec::new(),
    });
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(50.0))],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert!(h.host.state().get("lastSwitchAt").is_none());
}

// --- TestEventsShape ----------------------------------------------------------

#[test]
fn events_shape_test_every_event_has_envelope() {
    let h = Harness::three();
    let mut e = h.default_engine();
    h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(50.0))],
    );
    assert!(!h.events.borrow().is_empty());
    for event in h.events.borrow().iter() {
        let payload = event.to_json();
        assert_eq!(payload["schemaVersion"], 1);
        assert_eq!(payload["event"], event.name());
        assert!(payload["ts"].as_str().unwrap().ends_with('Z'));
    }
}

#[test]
fn events_shape_test_switch_event_refs_match_account_ref_shape() {
    let h = Harness::three();
    let mut e = h.default_engine();
    h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(50.0))],
    );
    let switch = h
        .events
        .borrow()
        .iter()
        .find(|e| e.name() == "switch")
        .unwrap()
        .to_json();
    assert_eq!(
        switch["from"],
        json!({"number": 1, "email": "a@example.com"})
    );
    assert_eq!(switch["to"], json!({"number": 2, "email": "b@example.com"}));
}

fn poll(h: &Harness) -> AutoSwitchEvent {
    h.events
        .borrow()
        .iter()
        .find(|e| e.name() == "poll")
        .unwrap()
        .clone()
}

#[test]
fn events_shape_test_poll_event_human_line() {
    let h = Harness::three();
    let mut e = h.default_engine();
    h.tick(
        &mut e,
        &[("1", usage(42.0)), ("2", usage(10.0)), ("3", None)],
    );
    let line = poll(&h).human();
    assert!(
        line.contains("Account-1") && line.contains("42% used"),
        "{line}"
    );
    assert!(line.contains("#2: 5h 10% · 7d 0%"), "{line}");
    assert!(line.contains("#3: ?"), "{line}");
}

#[test]
fn events_shape_test_poll_event_windows_match_the_decision_set() {
    let u = [
        ("1", usage(42.0)),
        (
            "2",
            Some(json!({
                "five_hour": {"pct": 3.0},
                "seven_day": {"pct": 89.0},
                "scoped": [{"name": "Fable", "pct": 21.0}],
            })),
        ),
    ];
    let plain = Harness::two();
    let mut e = plain.default_engine();
    plain.tick(&mut e, &u);
    let p = poll(&plain);
    assert!(p.human().contains("#2: 5h 3% · 7d 89%"));
    assert!(!p.human().contains("Fable"));
    assert_eq!(
        p.to_json()["windowsPct"]["2"],
        json!({"5h": 3.0, "7d": 89.0})
    );

    let modeled = Harness::two();
    let mut e = modeled.engine(with(|s| s.model = Some("Fable".into())));
    modeled.tick(&mut e, &u);
    let p = poll(&modeled);
    assert!(p.human().contains("#2: 5h 3% · 7d 89% · Fable 21%"));
    assert_eq!(
        p.to_json()["windowsPct"]["2"],
        json!({"5h": 3.0, "7d": 89.0, "Fable": 21.0})
    );
}

#[test]
fn events_shape_poll_json_matches_python_dumps() {
    let h = Harness::two();
    let mut e = h.default_engine();
    h.tick(&mut e, &[("1", usage(42.0)), ("2", None)]);
    assert_eq!(
        crate::pyjson::to_string_compact(&poll(&h).to_json()),
        "{\"schemaVersion\": 1, \"event\": \"poll\", \"ts\": \"1970-01-12T13:46:40Z\", \
         \"active\": {\"number\": 1, \"email\": \"a@example.com\"}, \
         \"headroomPct\": {\"1\": 58.0, \"2\": null}, \"threshold\": 90.0, \
         \"windowsPct\": {\"1\": {\"5h\": 42.0, \"7d\": 0.0}}}"
    );
}

// --- TestPctLabel (engine part) -----------------------------------------------

#[test]
fn pct_label_test_below_threshold_detail_shows_fractional_threshold() {
    let h = Harness::two();
    let mut e = h.engine(with(|s| s.threshold = 99.9));
    h.tick(&mut e, &[("1", usage(50.0)), ("2", usage(10.0))]);
    assert_eq!(h.details(), ["50% < 99.9%"]);
}

#[test]
fn pct_label_test_below_threshold_detail_never_shows_impossible_comparison() {
    let h = Harness::two();
    let mut e = h.engine(with(|s| s.threshold = 99.9));
    h.tick(&mut e, &[("1", usage(99.85)), ("2", usage(10.0))]);
    assert_eq!(h.details(), ["99.85% < 99.9%"]);
}

// --- TestModelAwareSwitch -----------------------------------------------------

fn model_engine<'h>(h: &'h Harness, model: Option<&str>) -> AutoSwitchEngine<'h> {
    h.engine(with(|s| s.model = model.map(str::to_string)))
}

#[test]
fn model_aware_test_model_maxed_switches_despite_session_headroom() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(5.0, 100.0)),
            ("2", model_usage(5.0, 30.0)),
            ("3", model_usage(5.0, 60.0)),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert_eq!(
        h.switch().unwrap().2,
        json!({"number": 2, "email": "b@example.com"})
    );
}

#[test]
fn model_aware_test_without_model_setting_the_same_usage_holds() {
    let h = Harness::three();
    let mut e = model_engine(&h, None);
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(5.0, 100.0)),
            ("2", model_usage(5.0, 30.0)),
            ("3", model_usage(5.0, 60.0)),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn model_aware_test_model_headroom_still_gated_by_session_window() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(100.0, 40.0)),
            ("2", model_usage(10.0, 40.0)),
            ("3", model_usage(20.0, 40.0)),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn model_aware_test_comma_separated_models_switch_on_any() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable,Opus"));
    let u = |five: f64, fable: f64, opus: f64| {
        Some(json!({
            "five_hour": {"pct": five},
            "seven_day": {"pct": 0.0},
            "scoped": [{"name": "Fable", "pct": fable}, {"name": "Opus", "pct": opus}],
        }))
    };
    let out = h.tick(
        &mut e,
        &[
            ("1", u(5.0, 20.0, 100.0)),
            ("2", u(5.0, 20.0, 30.0)),
            ("3", u(5.0, 20.0, 70.0)),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn model_aware_test_all_sentinel_binds_every_scoped_window() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("all"));
    let u = |name: &str, pct: f64| {
        Some(json!({"five_hour": {"pct": 5.0}, "seven_day": {"pct": 0.0},
                    "scoped": [{"name": name, "pct": pct}]}))
    };
    let out = h.tick(
        &mut e,
        &[
            ("1", u("Sonnet", 100.0)),
            ("2", u("Sonnet", 20.0)),
            ("3", u("Opus", 60.0)),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn model_aware_test_dual_exhausted_candidate_recovers_at_its_later_reset() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    let fable_reset = "2026-07-05T15:00:00Z";
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(95.0, 10.0)),
            (
                "2",
                Some(json!({
                    "five_hour": {"pct": 100.0, "resets_at": "2026-07-05T12:00:00Z"},
                    "seven_day": {"pct": 0.0},
                    "scoped": [{"name": "Fable", "pct": 100.0, "resets_at": fable_reset}],
                })),
            ),
            (
                "3",
                Some(json!({
                    "five_hour": {"pct": 100.0, "resets_at": "2026-07-05T20:00:00Z"},
                    "seven_day": {"pct": 0.0},
                })),
            ),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.exhausted_at(), Some(s(fable_reset)));
}

#[test]
fn model_aware_test_unknown_recovery_falls_back_instead_of_oversleeping() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(95.0, 10.0)),
            (
                "2",
                Some(json!({"five_hour": {"pct": 0.0}, "seven_day": {"pct": 0.0},
                            "scoped": [{"name": "Fable", "pct": 100.0}]})),
            ),
            (
                "3",
                Some(
                    json!({"five_hour": {"pct": 100.0, "resets_at": "2026-07-05T20:00:00Z"},
                            "seven_day": {"pct": 0.0}}),
                ),
            ),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.exhausted_at(), Some(None));
    assert_eq!(e.sleep_until_ts(), None);
    assert_eq!(e.next_delay(out), NO_RESET_FALLBACK_S);
}

#[test]
fn model_aware_test_scoped_only_exhaustion_drives_the_wake_time() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    let fable_reset = "2026-07-06T09:00:00Z";
    let blocked = Some(json!({
        "five_hour": {"pct": 3.0, "resets_at": "2026-07-05T12:00:00Z"},
        "seven_day": {"pct": 0.0},
        "scoped": [{"name": "Fable", "pct": 100.0, "resets_at": fable_reset}],
    }));
    let out = h.tick(
        &mut e,
        &[
            ("1", model_usage(95.0, 10.0)),
            ("2", blocked.clone()),
            ("3", blocked),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.exhausted_at(), Some(s(fable_reset)));
}

#[test]
fn model_aware_test_unmatched_model_name_warns_once() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fabel"));
    let u = [
        ("1", model_usage(5.0, 10.0)),
        ("2", model_usage(5.0, 10.0)),
        ("3", model_usage(5.0, 10.0)),
    ];
    h.tick(&mut e, &u);
    assert_eq!(h.count("config-warning"), 1);
    let warning = h
        .events
        .borrow()
        .iter()
        .find(|e| e.name() == "config-warning")
        .unwrap()
        .clone();
    assert!(warning.human().contains("Fabel"));
    assert_eq!(warning.to_json()["event"], "config-warning");
    h.tick(&mut e, &u);
    assert_eq!(h.count("config-warning"), 1);
}

#[test]
fn model_aware_test_no_false_warning_while_an_account_is_unreadable() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fabel"));
    h.tick(
        &mut e,
        &[
            ("1", model_usage(5.0, 10.0)),
            ("2", model_usage(5.0, 10.0)),
            ("3", None),
        ],
    );
    assert!(!h.has("config-warning"));
    h.tick(
        &mut e,
        &[
            ("1", model_usage(5.0, 10.0)),
            ("2", model_usage(5.0, 10.0)),
            ("3", model_usage(5.0, 10.0)),
        ],
    );
    assert!(h.has("config-warning"));
}

#[test]
fn model_aware_test_matching_name_never_warns() {
    let h = Harness::three();
    let mut e = model_engine(&h, Some("Fable"));
    h.tick(
        &mut e,
        &[
            ("1", model_usage(5.0, 10.0)),
            ("2", model_usage(5.0, 10.0)),
            ("3", model_usage(5.0, 10.0)),
        ],
    );
    assert!(!h.has("config-warning"));
}

// --- TestConsumeFirstStrategy -------------------------------------------------

const R_PAST: &str = "1970-01-10T00:00:00Z";
const R_SOON: &str = "2024-01-05T00:00:00Z";
const R_LATER: &str = "2024-01-08T00:00:00Z";
const R_LATEST: &str = "2024-01-10T00:00:00Z";

fn consume_first(h: &Harness) -> AutoSwitchEngine<'_> {
    h.engine(with(|s| s.strategy = AutoStrategy::ConsumeFirst))
}

#[test]
fn consume_first_test_below_threshold_switches_to_soonest_weekly_reset() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    let (trigger, _, to, _) = h.switch().unwrap();
    assert_eq!(trigger, Trigger::ConsumeFirst);
    assert_eq!(to, json!({"number": 2, "email": "b@example.com"}));
}

#[test]
fn consume_first_test_stays_when_active_already_resets_soonest() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_SOON))),
            ("2", usage7(10.0, 10.0, Some(R_LATER))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["already-consuming-soonest"]);
}

#[test]
fn consume_first_test_over_threshold_prefers_soonest_reset_over_max_headroom() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(95.0, 20.0, Some(R_LATER))),
            ("2", usage7(50.0, 40.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn consume_first_test_a_consume_first_target_must_still_be_healthy() {
    let h = Harness::two();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(40.0, 40.0, Some(R_LATEST))),
            ("2", usage7(96.0, 96.0, Some(R_SOON))),
        ],
    );
    assert_ne!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn consume_first_test_respects_cooldown() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(h.active(), s("2"));
    h.events.borrow_mut().clear();
    let out = h.tick(
        &mut e,
        &[
            ("2", usage7(20.0, 20.0, Some(R_LATER))),
            ("1", usage7(10.0, 10.0, Some(R_LATEST))),
            ("3", usage7(10.0, 10.0, Some(R_SOON))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.active(), s("2"));
    assert!(h.reasons().contains(&"cooldown"));
}

#[test]
fn consume_first_test_locked_recheck_stops_concurrent_engine() {
    // The loser reads the state before the winner commits; only the
    // recheck under the state lock can stop it.
    let h = Harness::three();
    let mut loser = consume_first(&h);
    let state = StateFile::new(h.host.root.path().join(STATE_FILENAME));
    let now = h.clock.now();
    let view = [
        ("1", usage7(10.0, 10.0, Some(R_LATEST)).unwrap()),
        ("2", usage7(20.0, 20.0, Some(R_LATER)).unwrap()),
        ("3", usage7(10.0, 10.0, Some(R_SOON)).unwrap()),
    ];
    let winner_committed = Cell::new(false);
    *h.host.current.borrow_mut() = Some("2".into());
    h.host.serve(Box::new(move |_| {
        if !winner_committed.replace(true) {
            state
                .mutate(|s| {
                    s.insert("lastSwitchAt".into(), json!(now));
                    s.insert("lastSwitchTo".into(), json!("2"));
                })
                .unwrap();
        }
        view.iter()
            .map(|(n, v)| (n.to_string(), entry_for(&Val::Dict(v.clone()), now)))
            .collect()
    }));
    assert_eq!(loser.tick(), TickOutcome::NoAction);
    assert_eq!(h.active(), s("2"));
    assert!(h.reasons().contains(&"cooldown"));
}

#[test]
fn consume_first_test_reset_unknown_when_active_reset_missing() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, None)),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["reset-unknown"]);
}

#[test]
fn consume_first_test_unreadable_candidates_stay_no_comparison() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", None),
            ("3", None),
        ],
    );
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.reasons(), ["no-comparison"]);
}

#[test]
fn consume_first_test_exhausted_candidates_hold_without_false_reset_claim() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(100.0, 100.0, Some(R_SOON))),
            ("3", usage7(100.0, 100.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["already-consuming-soonest"]);
    assert_eq!(
        h.details(),
        ["no sooner-resetting account with room to spare"]
    );
}

#[test]
fn consume_first_test_single_account_below_threshold_is_no_action() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.make_live("a@example.com");
    let mut e = consume_first(&h);
    let out = h.tick(&mut e, &[("1", usage7(20.0, 20.0, Some(R_SOON)))]);
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn consume_first_test_api_key_only_peers_below_threshold_is_no_action() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed(2, "key@token.local");
    h.host.make_live("a@example.com");
    h.host.mark_api_key(2);
    let mut e = h.engine(with(|s| {
        s.strategy = AutoStrategy::ConsumeFirst;
        s.include_api_key_accounts = true;
    }));
    h.serve_values(vec![
        (
            "1".into(),
            Val::Dict(usage7(20.0, 20.0, Some(R_SOON)).unwrap()),
        ),
        ("2".into(), Val::Sentinel(Sentinel::ApiKey)),
    ]);
    assert_eq!(e.tick(), TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn consume_first_test_skips_sooner_account_that_is_exhausted() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATEST))),
            ("2", usage7(100.0, 100.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATER))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn consume_first_test_best_strategy_unaffected_below_threshold() {
    let h = Harness::two();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["below-threshold"]);
}

#[test]
fn consume_first_test_candidate_with_past_reset_is_not_selected() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_PAST))),
            ("3", usage7(10.0, 10.0, Some(R_SOON))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
    assert_eq!(
        h.switch().unwrap().2,
        json!({"number": 3, "email": "c@example.com"})
    );
}

#[test]
fn consume_first_test_active_past_reset_holds_reset_unknown() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_PAST))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATER))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.reasons(), ["reset-unknown"]);
}

type View = Vec<(&'static str, Option<Value>)>;

/// Stored-snapshot collections serve `stored`, the all-candidates
/// escalation serves `fresh`.
fn two_phase_tick(h: &Harness, e: &mut AutoSwitchEngine, stored: View, fresh: View) -> TickOutcome {
    let clock = h.clock.clone();
    let all: HashSet<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
    h.host.serve(Box::new(move |fetch| {
        let view = if *fetch == all { &fresh } else { &stored };
        view.iter()
            .map(|(n, v)| {
                (
                    n.to_string(),
                    entry_for(&Val::Dict(v.clone().unwrap()), clock.now()),
                )
            })
            .collect()
    }));
    e.tick()
}

fn all_fetches(h: &Harness) -> usize {
    let all: HashSet<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
    h.host
        .fetch_sets
        .borrow()
        .iter()
        .filter(|f| **f == all)
        .count()
}

#[test]
fn consume_first_test_two_phase_refetch_disqualifies_stale_pick() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = two_phase_tick(
        &h,
        &mut e,
        vec![
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
        vec![
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(100.0, 100.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
    assert_eq!(h.reasons(), ["already-consuming-soonest"]);
    assert_eq!(all_fetches(&h), 1);
}

#[test]
fn consume_first_test_two_phase_refetch_confirms_switch() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let view = vec![
        ("1", usage7(20.0, 20.0, Some(R_LATER))),
        ("2", usage7(10.0, 10.0, Some(R_SOON))),
        ("3", usage7(10.0, 10.0, Some(R_LATEST))),
    ];
    let out = two_phase_tick(&h, &mut e, view.clone(), view);
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert!(all_fetches(&h) >= 1);
}

#[test]
fn consume_first_test_two_phase_refetch_reranks_to_fresh_best() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let out = two_phase_tick(
        &h,
        &mut e,
        vec![
            ("1", usage7(20.0, 20.0, Some(R_LATEST))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATER))),
        ],
        vec![
            ("1", usage7(20.0, 20.0, Some(R_LATEST))),
            ("2", usage7(10.0, 10.0, Some(R_LATER))),
            ("3", usage7(10.0, 10.0, Some(R_SOON))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn consume_first_test_threshold_crossed_in_phase_two_holds_then_escapes_next_tick() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let fresh = vec![
        ("1", usage7(100.0, 20.0, Some(R_LATER))),
        ("2", usage7(10.0, 10.0, Some(R_LATEST))),
        ("3", usage7(10.0, 10.0, Some(R_LATEST))),
    ];
    let out = two_phase_tick(
        &h,
        &mut e,
        vec![
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
        fresh.clone(),
    );
    assert_eq!(out, TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
    assert!(!h.has("switch"));
    assert_eq!(all_fetches(&h), 1);
    assert_eq!(h.reasons(), ["already-consuming-soonest"]);
    h.events.borrow_mut().clear();
    assert_eq!(h.tick(&mut e, &fresh), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert_eq!(h.switch().unwrap().0, Trigger::AtLimit);
}

// --- state written by a switch ------------------------------------------------

#[test]
fn a_switch_records_where_it_came_from_and_what_it_left() {
    let h = Harness::three();
    let mut e = h.default_engine();
    h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(40.0)), ("3", usage(20.0))],
    );
    let state = h.host.state();
    assert_eq!(
        state,
        json!({
            "schemaVersion": 1,
            "lastSwitchAt": START,
            "lastSwitchTo": "3",
            "lastSwitchFrom": 1,
            "leftHeadroom": 5.0,
            "leftRecoveryAt": null,
            "leftTrigger": "proactive",
        })
    );
    let keys: Vec<&String> = state.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "schemaVersion",
            "lastSwitchAt",
            "lastSwitchTo",
            "lastSwitchFrom",
            "leftHeadroom",
            "leftRecoveryAt",
            "leftTrigger"
        ]
    );
}

#[test]
fn iso_utc_keeps_microseconds_only_when_present() {
    assert_eq!(iso_utc(1_783_074_600.0), "2026-07-03T10:30:00Z");
    assert_eq!(iso_utc(1_783_074_600.5), "2026-07-03T10:30:00.500000Z");
}

mod no_flap;
mod no_return;
mod safety;
