//! The loop, its cadence and the scheduler it drives (upstream
//! `TestRunLoop`, `TestLoopObeysThePollPlan`, `TestSessionThreshold`,
//! `TestAdaptiveScheduler`).

use std::time::{Duration, Instant};

use super::*;
use crate::poll_policy::{PlanInputs, plan_after_fetch};
use crate::usage_store::{FetchRecord, Identity, UsageStore};

/// Calls `on_event(n, event, handle)` with the 1-based count of events of
/// that kind so far.
fn looping_engine<'h>(
    h: &'h Harness,
    host: &'h dyn EngineHost,
    settings: AutoSwitchSettings,
    mut on_event: impl FnMut(usize, &AutoSwitchEvent, &LoopHandle) + 'h,
) -> AutoSwitchEngine<'h> {
    let handle = LoopHandle::new();
    let events = h.events.clone();
    let inner = handle.clone();
    let mut seen: HashMap<&'static str, usize> = HashMap::new();
    AutoSwitchEngine::new(
        host,
        settings,
        Box::new(move |e| {
            let n = seen.entry(e.name()).or_default();
            *n += 1;
            on_event(*n, &e, &inner);
            events.borrow_mut().push(e);
        }),
    )
    .clock(h.clock.clock())
    .rng(Box::new(|| 0.5))
    .loop_handle(handle)
}

fn fast() -> AutoSwitchSettings {
    with(|s| s.interval_seconds = 0.001)
}

fn stop_on_poll(n: usize) -> impl FnMut(usize, &AutoSwitchEvent, &LoopHandle) {
    move |count, e, handle| {
        if e.name() == "poll" && count >= n {
            handle.stop();
        }
    }
}

// --- TestRunLoop --------------------------------------------------------------

#[test]
fn run_loop_test_loop_ticks_until_stopped() {
    let h = Harness::three();
    h.serve(&[("1", usage(10.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    let mut engine = looping_engine(&h, &h.host, fast(), stop_on_poll(2));
    assert_eq!(engine.run_loop(), 0);
    assert_eq!(h.count("poll"), 2);
    assert!(!h.has("sleep"));
}

#[test]
fn run_loop_test_loop_survives_failing_tick() {
    let h = Harness::three();
    h.serve(&[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    let blocker = h.host.root.path().join("not-a-dir");
    std::fs::write(&blocker, "").unwrap();
    let mut engine = looping_engine(&h, &h.host, fast(), stop_on_poll(2))
        .state_file(StateFile::new(blocker.join(STATE_FILENAME)));
    assert_eq!(engine.run_loop(), 0);
    assert_eq!(h.count("poll"), 2);
    assert_eq!(h.count("error"), 2);
}

#[test]
fn run_loop_test_stop_before_start_is_not_lost() {
    let h = Harness::three();
    let mut engine = looping_engine(&h, &h.host, AutoSwitchSettings::default(), |_, _, _| {});
    engine.stop();
    assert_eq!(engine.run_loop(), 0);
    assert!(h.events.borrow().is_empty());
}

#[test]
fn run_loop_test_stop_from_another_thread_ends_the_sleep() {
    let h = Harness::three();
    h.serve(&[("1", usage(10.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    let mut engine = looping_engine(&h, &h.host, AutoSwitchSettings::default(), |_, _, _| {});
    let handle = engine.handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        handle.stop();
    });
    let started = Instant::now();
    assert_eq!(engine.run_loop(), 0);
    stopper.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(h.count("poll"), 1);
}

#[test]
fn run_loop_test_wake_during_tick_cuts_the_following_sleep_short() {
    let h = Harness::three();
    h.serve(&[("1", usage(10.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    let mut engine = looping_engine(
        &h,
        &h.host,
        AutoSwitchSettings::default(),
        |n, e, handle| match (e.name(), n) {
            ("poll", 1) => handle.wake(),
            ("poll", _) => handle.stop(),
            _ => {}
        },
    );
    let started = Instant::now();
    assert_eq!(engine.run_loop(), 0);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(h.count("poll"), 2);
}

#[test]
fn run_loop_test_blocked_with_reset_rechecks_at_exhausted_cadence() {
    let h = Harness::three();
    let mut engine = h.default_engine();
    engine.sleep_until_ts = Some(h.clock.now() + 1800.0);
    assert_eq!(
        engine.next_delay(TickOutcome::Blocked),
        poll_policy::EXHAUSTED_INTERVAL_S
    );
}

#[test]
fn run_loop_test_blocked_exhausted_without_reset_uses_fallback() {
    let h = Harness::three();
    let mut engine = h.default_engine();
    engine.sleep_until_ts = None;
    engine.blocked_wait_long = true;
    assert_eq!(engine.next_delay(TickOutcome::Blocked), 300.0);
}

#[test]
fn run_loop_test_blocked_on_resolvable_condition_keeps_normal_cadence() {
    let h = Harness::three();
    let mut engine = h.engine(AutoSwitchSettings::default());
    engine.rng = Box::new(|| 0.999);
    let delay = engine.next_delay(TickOutcome::Blocked);
    assert!((0.9 * 60.0..=1.1 * 60.0).contains(&delay), "{delay}");
}

#[test]
fn run_loop_test_normal_delay_is_jittered_interval() {
    let h = Harness::three();
    for (r, want) in [(0.0, 54.0), (0.5, 60.0), (0.999_999, 66.0)] {
        let mut engine = h.default_engine();
        engine.rng = Box::new(move || r);
        let delay = engine.next_delay(TickOutcome::NoAction);
        assert!((delay - want).abs() < 1e-3, "{r}: {delay}");
    }
}

#[test]
fn run_loop_test_sleep_cap() {
    let h = Harness::three();
    let mut engine = h.default_engine();
    engine.sleep_until_ts = Some(h.clock.now() + 50.0 * 3600.0);
    assert_eq!(
        engine.next_delay(TickOutcome::Blocked),
        poll_policy::EXHAUSTED_INTERVAL_S
    );
}

#[test]
fn run_loop_all_exhausted_sleeps_toward_the_earliest_reset() {
    let h = Harness::two();
    let reset = iso_utc(START + 1800.0);
    h.serve(&[
        ("1", usage_reset(100.0, &reset)),
        ("2", usage_reset(100.0, &reset)),
    ]);
    let mut engine = looping_engine(
        &h,
        &h.host,
        AutoSwitchSettings::default(),
        |_, e, handle| {
            if e.name() == "all-exhausted" {
                handle.stop();
            }
        },
    );
    assert_eq!(engine.run_loop(), 0);
    let names: Vec<&str> = h.events.borrow().iter().map(|e| e.name()).collect();
    assert_eq!(names, ["poll", "all-exhausted", "sleep"]);
    let sleep = h.events.borrow().last().unwrap().to_json();
    assert_eq!(sleep["seconds"], json!(600.0));
    assert_eq!(sleep["until"], json!(iso_utc(START + 600.0)));
    assert_eq!(
        h.events.borrow().last().unwrap().human(),
        format!("sleeping 10m (until {})", iso_utc(START + 600.0))
    );
}

#[test]
fn run_loop_short_waits_emit_no_sleep_event() {
    let h = Harness::two();
    h.serve(&[("1", usage(95.0)), ("2", usage(95.0))]);
    let mut engine = looping_engine(
        &h,
        &h.host,
        with(|s| s.interval_seconds = 0.001),
        stop_on_poll(1),
    );
    assert_eq!(engine.run_loop(), 0);
    assert!(h.has("no-switch"));
    assert!(!h.has("sleep"));
}

// --- TestLoopObeysThePollPlan ---------------------------------------------------

fn plan_due_in(h: &Harness, due_in: f64) {
    let entry = UsageEntry {
        last_good: usage(10.0),
        fetched_at: Some(h.clock.now()),
        age_s: Some(0.0),
        next_poll_at: Some(h.clock.now() + due_in),
        ..Default::default()
    };
    h.serve_entries(vec![("1", entry)]);
}

#[test]
fn loop_obeys_the_poll_plan_test_sleep_is_cut_to_the_rows_next_poll() {
    let h = Harness::three();
    plan_due_in(&h, 60.0);
    let mut engine = h.engine(with(|s| s.interval_seconds = 360.0));
    assert_eq!(engine.next_delay(TickOutcome::NoAction), 60.0);
}

#[test]
fn loop_obeys_the_poll_plan_test_never_sleeps_below_the_planners_own_floor() {
    let h = Harness::three();
    plan_due_in(&h, -500.0);
    let mut engine = h.engine(with(|s| s.interval_seconds = 360.0));
    assert_eq!(
        engine.next_delay(TickOutcome::NoAction),
        poll_policy::URGENT_INTERVAL_S
    );
}

#[test]
fn loop_obeys_the_poll_plan_test_a_relaxed_plan_never_lengthens_the_sleep() {
    let h = Harness::three();
    plan_due_in(&h, 3600.0);
    let mut engine = h.engine(with(|s| s.interval_seconds = 60.0));
    assert!(engine.next_delay(TickOutcome::NoAction) <= 1.1 * 60.0);
}

#[test]
fn loop_obeys_the_poll_plan_a_short_interval_is_never_raised_to_the_floor() {
    let h = Harness::three();
    plan_due_in(&h, -500.0);
    let mut engine = h.engine(with(|s| s.interval_seconds = 15.0));
    assert_eq!(engine.next_delay(TickOutcome::NoAction), 15.0);
}

// --- TestSessionThreshold -------------------------------------------------------

#[test]
fn session_threshold_test_apply_threshold_retargets_trigger_and_poll_pin() {
    let h = Harness::three();
    let host = StoreHost::fake(&h.host);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    assert_eq!(*host.poll_inputs.borrow(), Some((90.0, Vec::new())));
    engine.apply_threshold(72.0);
    assert_eq!(engine.settings().threshold, 72.0);
    assert_eq!(*host.poll_inputs.borrow(), Some((72.0, Vec::new())));
    let outcome = h.tick(
        &mut engine,
        &[("1", usage(80.0)), ("2", usage(10.0)), ("3", usage(10.0))],
    );
    assert_eq!(outcome, TickOutcome::Switched);
}

#[test]
fn session_threshold_handle_threshold_lands_on_the_next_pass() {
    let h = Harness::three();
    h.serve(&[("1", usage(80.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    let host = StoreHost::fake(&h.host);
    let mut engine = looping_engine(&h, &host, fast(), stop_on_poll(1));
    engine.handle().apply_threshold(72.0);
    assert_eq!(engine.run_loop(), 0);
    assert_eq!(engine.settings().threshold, 72.0);
    assert_eq!(*host.poll_inputs.borrow(), Some((72.0, Vec::new())));
    assert!(h.has("switch"));
}

fn collect_fetch_sets(threshold: f64) -> Vec<HashSet<String>> {
    let h = Harness::three();
    let mut engine = h.default_engine();
    h.serve(&[("1", usage(80.0)), ("2", usage(10.0)), ("3", usage(10.0))]);
    engine.collect_scheduled_usage("1", &HashSet::new(), threshold);
    h.host.fetch_sets.borrow().clone()
}

#[test]
fn session_threshold_test_collect_escalates_on_the_tick_snapshot_threshold() {
    let all: HashSet<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
    assert!(collect_fetch_sets(90.0).contains(&all));
    assert!(!collect_fetch_sets(99.9).contains(&all));
}

// --- TestAdaptiveScheduler ------------------------------------------------------

/// The fake host with the real usage store behind `usage_entries`: fetches
/// are counted and planned like the collector does, with a jitter of 0.
struct StoreHost<'h> {
    inner: &'h FakeHost,
    store: Option<UsageStore>,
    usage: RefCell<HashMap<String, Option<Value>>>,
    errors: RefCell<HashMap<String, String>>,
    counts: RefCell<HashMap<String, usize>>,
    poll_inputs: RefCell<Option<(f64, Vec<String>)>>,
    fetch_sets: RefCell<Vec<HashSet<String>>>,
}

impl<'h> StoreHost<'h> {
    fn fake(inner: &'h FakeHost) -> Self {
        StoreHost {
            inner,
            store: None,
            usage: RefCell::default(),
            errors: RefCell::default(),
            counts: RefCell::default(),
            poll_inputs: RefCell::default(),
            fetch_sets: RefCell::default(),
        }
    }

    fn real(inner: &'h FakeHost, clock: Clock) -> Self {
        let store = UsageStore::with_clock(&inner.root.path().join("cache"), clock);
        StoreHost {
            store: Some(store),
            ..Self::fake(inner)
        }
    }

    fn identities(&self) -> Vec<(String, Identity)> {
        self.inner
            .slots
            .borrow()
            .iter()
            .map(|(num, email, _)| (num.clone(), (email.clone(), String::new())))
            .collect()
    }

    fn set(&self, num: &str, value: Option<Value>) {
        self.usage.borrow_mut().insert(num.into(), value);
    }

    fn count(&self, num: &str) -> usize {
        self.counts.borrow().get(num).copied().unwrap_or(0)
    }

    fn counts(&self) -> Vec<(String, usize)> {
        let mut v: Vec<(String, usize)> = self
            .counts
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.sort();
        v
    }

    fn entry(&self, num: &str) -> UsageEntry {
        let identities = self.identities();
        self.store
            .as_ref()
            .unwrap()
            .entries(&identities, &[])
            .remove(num)
            .unwrap()
    }

    fn set_plan(&self, num: &str, next: f64, interval: f64) {
        self.store
            .as_ref()
            .unwrap()
            .set_poll_plan(&[(num.into(), (next, interval))], &self.identities())
            .unwrap();
    }

    fn fetch(&self, num: &str) -> FetchRecord {
        *self.counts.borrow_mut().entry(num.into()).or_default() += 1;
        if let Some(error) = self.errors.borrow().get(num) {
            return FetchRecord::error(error.clone(), None);
        }
        FetchRecord {
            usage: self.usage.borrow().get(num).cloned().flatten(),
            ..Default::default()
        }
    }
}

impl EngineHost for StoreHost<'_> {
    fn backup_root(&self) -> PathBuf {
        self.inner.backup_root()
    }
    fn current_account_number(&self) -> Option<String> {
        self.inner.current_account_number()
    }
    fn has_live_login(&self) -> bool {
        self.inner.has_live_login()
    }
    fn account_email(&self, num: &str) -> String {
        self.inner.account_email(num)
    }
    fn account_kind(&self, num: &str) -> AccountKind {
        self.inner.account_kind(num)
    }
    fn switchable_account_numbers(&self) -> Vec<String> {
        self.inner.switchable_account_numbers()
    }
    fn account_identity(&self, num: &str) -> crate::store::SlotIdentity {
        self.inner.account_identity(num)
    }
    fn backfill_account_uuid(&self, num: &str, uuid: &str) {
        self.inner.backfill_account_uuid(num, uuid)
    }
    fn usage_entries(&self, fetch: &HashSet<String>, scheduled: bool) -> Vec<(String, UsageEntry)> {
        self.fetch_sets.borrow_mut().push(fetch.clone());
        let Some(store) = &self.store else {
            return self.inner.usage_entries(fetch, scheduled);
        };
        let identities = self.identities();
        let (threshold, models) = self
            .poll_inputs
            .borrow()
            .clone()
            .unwrap_or((90.0, Vec::new()));
        let pre = store.entries(&identities, &models);
        let requested: Vec<String> = identities
            .iter()
            .map(|(n, _)| n.clone())
            .filter(|n| fetch.contains(n))
            .collect();
        let claims = store
            .reserve(&requested, &identities, false, scheduled)
            .unwrap();
        if !claims.is_empty() {
            let now = store.now();
            let current = self.current_account_number();
            let records: Vec<(String, FetchRecord)> = claims
                .iter()
                .map(|(n, _)| (n.clone(), self.fetch(n)))
                .collect();
            let plans: HashMap<String, (f64, f64)> = records
                .iter()
                .filter(|(_, r)| r.error.is_none())
                .map(|(num, record)| {
                    let before = pre.get(num);
                    let plan = plan_after_fetch(
                        &PlanInputs {
                            prev_interval_s: before.and_then(|b| b.poll_interval_s),
                            prev_usage: before.and_then(|b| b.last_good.as_ref()),
                            new_usage: record.usage.as_ref(),
                            is_active: current.as_deref() == Some(num),
                            threshold,
                            models: &models,
                            recent_429: before.is_some_and(|b| b.recent_429(now)),
                            now,
                        },
                        &mut || 0.5,
                    );
                    (num.clone(), plan)
                })
                .collect();
            let claim_map: HashMap<String, String> = claims.into_iter().collect();
            store
                .record(&records, &identities, Some(&claim_map), &plans)
                .unwrap();
        }
        let mut entries = store.entries(&identities, &models);
        identities
            .iter()
            .map(|(n, _)| (n.clone(), entries.remove(n).unwrap_or_default()))
            .collect()
    }
    fn read_account_credentials(&self, num: &str, email: &str) -> String {
        self.inner.read_account_credentials(num, email)
    }
    fn live_session_pids_for(&self, num: &str, email: &str) -> Vec<i32> {
        self.inner.live_session_pids_for(num, email)
    }
    fn consume_backup_grant(&self, num: &str, email: &str, snapshot: &str) -> RefreshOutcome {
        self.inner.consume_backup_grant(num, email, snapshot)
    }
    fn switch_to(&self, num: &str) -> crate::error::Result<SwitchResult> {
        self.inner.switch_to(num)
    }
    fn set_poll_policy_inputs(&self, threshold: f64, models: &[String]) {
        *self.poll_inputs.borrow_mut() = Some((threshold, models.to_vec()));
    }
}

impl Harness {
    fn engine_on<'h>(
        &'h self,
        host: &'h dyn EngineHost,
        settings: AutoSwitchSettings,
    ) -> AutoSwitchEngine<'h> {
        let events = self.events.clone();
        AutoSwitchEngine::new(
            host,
            settings,
            Box::new(move |e| events.borrow_mut().push(e)),
        )
        .clock(self.clock.clock())
        .rng(Box::new(|| 0.5))
    }
}

fn scheduler(accounts: u32) -> Harness {
    let h = Harness::bare();
    for (num, email) in [
        (1, "a@example.com"),
        (2, "b@example.com"),
        (3, "c@example.com"),
    ]
    .into_iter()
    .take(accounts as usize)
    {
        h.host.seed(num, email);
    }
    h.host.make_live("a@example.com");
    h
}

fn served(host: &StoreHost, usage: &[(&str, Option<Value>)]) {
    for (n, v) in usage {
        host.set(n, v.clone());
    }
}

fn counts(pairs: &[(&str, usize)]) -> Vec<(String, usize)> {
    pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
}

#[test]
fn adaptive_scheduler_test_baseline_fetches_active_plus_one_candidate() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(20.0))],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1)]));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.counts(), counts(&[("1", 2), ("2", 1), ("3", 1)]));
}

#[test]
fn adaptive_scheduler_test_near_threshold_escalates_to_full_refresh() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[("1", usage(80.0)), ("2", usage(10.0)), ("3", usage(20.0))],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    assert_eq!(engine.tick(), TickOutcome::NoAction);
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
}

#[test]
fn adaptive_scheduler_test_active_unknown_escalates_before_failover() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("2", usage(10.0)), ("3", usage(50.0))]);
    host.errors
        .borrow_mut()
        .insert("1".into(), "timeout".into());
    let mut engine = h.engine_on(&host, with(|s| s.unhealthy_ticks = 1));
    assert_eq!(engine.tick(), TickOutcome::Switched);
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
    assert_eq!(h.active(), s("2"));
}

#[test]
fn adaptive_scheduler_test_active_cadence_floor_and_decay() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(10.0)), ("2", usage(20.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    assert_eq!(host.count("1"), 1);
    for _ in 0..2 {
        h.clock.advance(60.0);
        engine.tick();
    }
    assert_eq!(host.count("1"), 1);
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    h.clock.advance(240.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 3);
}

#[test]
fn adaptive_scheduler_test_urgent_cadence_when_burning_near_the_band() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(70.0)), ("2", usage(10.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    host.set("1", usage(80.0));
    h.clock.advance(180.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    host.set("1", usage(84.0));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 3);
}

#[test]
fn adaptive_scheduler_test_in_band_without_movement_keeps_the_floor() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(80.0)), ("2", usage(10.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    for _ in 0..2 {
        h.clock.advance(60.0);
        engine.tick();
    }
    assert_eq!(host.count("1"), 1);
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
}

#[test]
fn adaptive_scheduler_test_urgent_band_follows_the_threshold() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(30.0)), ("2", usage(10.0))]);
    let mut engine = h.engine_on(&host, with(|s| s.threshold = 50.0));
    engine.tick();
    host.set("1", usage(40.0));
    h.clock.advance(180.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    host.set("1", usage(44.0));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 3);
}

#[test]
fn adaptive_scheduler_test_stale_candidate_plan_never_gates_the_active() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(50.0)), ("2", usage(20.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    host.set_plan("1", h.clock.now() + 600.0, 600.0);
    h.clock.advance(240.0);
    engine.tick();
    assert_eq!(host.count("1"), 1);
    h.clock.advance(120.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
}

#[test]
fn adaptive_scheduler_test_exhausted_active_is_rechecked_before_its_reset() {
    let h = scheduler(1);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[("1", usage_reset(100.0, &iso_utc(START + 7200.0)))],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    assert_eq!(host.count("1"), 1);
    for _ in 0..3 {
        h.clock.advance(400.0);
        engine.tick();
    }
    assert_eq!(host.count("1"), 2);
}

#[test]
fn adaptive_scheduler_test_band_jump_is_seen_at_most_one_poll_late() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(40.0)), ("2", usage(20.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    host.set("1", usage(80.0));
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 1);
    h.clock.advance(120.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    assert_eq!(host.count("2"), 1);
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.count("1"), 3);
    assert_eq!(host.count("2"), 2);
}

#[test]
fn adaptive_scheduler_test_active_in_backoff_keeps_trusted_headroom() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(20.0))],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    h.clock.advance(60.0);
    engine.tick();
    let identities = host.identities();
    host.store
        .as_ref()
        .unwrap()
        .record(
            &[("1".into(), FetchRecord::error("http-429", Some(600.0)))],
            &identities,
            None,
            &HashMap::new(),
        )
        .unwrap();
    h.clock.advance(400.0);
    host.counts.borrow_mut().clear();
    assert_eq!(engine.tick(), TickOutcome::NoAction);
    assert_eq!(engine.unhealthy_ticks, 0);
    assert_eq!(host.count("1"), 0);
    assert_eq!(host.counts().iter().map(|(_, c)| c).sum::<usize>(), 1);
}

#[test]
fn adaptive_scheduler_test_all_exhausted_escalation_preserves_wider_plan() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[
            ("1", usage(100.0)),
            ("2", usage(100.0)),
            ("3", usage(100.0)),
        ],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    assert_eq!(engine.tick(), TickOutcome::Blocked);
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
    host.set_plan("2", h.clock.now() + 1800.0, 1800.0);
    h.clock.advance(NO_RESET_FALLBACK_S);
    assert_eq!(engine.tick(), TickOutcome::Blocked);
    assert_eq!(host.count("2"), 1);
}

#[test]
fn adaptive_scheduler_test_exhausted_candidate_keeps_a_bounded_poll_plan() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[
            ("1", usage(50.0)),
            ("2", usage_reset(100.0, "2026-07-05T12:00:00Z")),
            ("3", usage(20.0)),
        ],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    for _ in 0..3 {
        engine.tick();
        h.clock.advance(60.0);
    }
    assert_eq!(host.count("2"), 1);
    let entry = host.entry("2");
    assert_eq!(
        entry.poll_interval_s,
        Some(poll_policy::EXHAUSTED_INTERVAL_S)
    );
    assert!(
        entry.next_poll_at.unwrap()
            <= entry.fetched_at.unwrap()
                + poll_policy::EXHAUSTED_INTERVAL_S * (1.0 + poll_policy::JITTER_FRAC)
    );
}

#[test]
fn adaptive_scheduler_test_poll_never_scheduled_past_a_window_reset() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    let reset = START + 90.0;
    served(
        &host,
        &[
            ("1", usage(50.0)),
            ("2", usage_reset(40.0, &iso_utc(reset))),
        ],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    let entry = host.entry("2");
    assert!((entry.next_poll_at.unwrap() - (reset + RESET_SLACK_S)).abs() < 1e-6);
    assert_eq!(
        entry.poll_interval_s,
        Some(poll_policy::CANDIDATE_DEFAULT_INTERVAL_S)
    );
}

#[test]
fn adaptive_scheduler_test_movement_adapts_poll_interval() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("1", usage(50.0)), ("2", usage(10.0))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    let interval = || host.entry("2").poll_interval_s;
    engine.tick();
    assert_eq!(interval(), Some(poll_policy::CANDIDATE_DEFAULT_INTERVAL_S));
    h.clock.advance(180.0);
    engine.tick();
    assert_eq!(host.count("2"), 1);
    h.clock.advance(120.0);
    engine.tick();
    assert_eq!(host.count("2"), 2);
    assert_eq!(interval(), Some(450.0));
    h.clock.advance(450.0);
    host.set("2", usage(20.0));
    engine.tick();
    assert_eq!(host.count("2"), 3);
    assert_eq!(interval(), Some(225.0));
}

#[test]
fn adaptive_scheduler_test_poll_event_carries_fetch_errors() {
    let h = scheduler(2);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(&host, &[("2", usage(10.0))]);
    host.errors
        .borrow_mut()
        .insert("1".into(), "http-429".into());
    let mut engine = h.engine_on(&host, with(|s| s.unhealthy_ticks = 3));
    engine.tick();
    let poll = h
        .events
        .borrow()
        .iter()
        .find(|e| e.name() == "poll")
        .cloned()
        .unwrap();
    assert!(poll.human().contains("http-429"));
    assert_eq!(poll.to_json()["fetchErrors"], json!({"1": "http-429"}));
}

#[test]
fn adaptive_scheduler_test_quarantined_candidate_never_consumes_the_poll_slot() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    served(
        &host,
        &[("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(20.0))],
    );
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine
        .quarantine("2", "b@example.com", "invalid_grant")
        .unwrap();
    for _ in 0..3 {
        engine.tick();
        h.clock.advance(60.0);
    }
    assert_eq!(host.count("2"), 0);
    assert!(host.count("3") >= 1);
}

#[test]
fn adaptive_scheduler_test_consume_first_hold_never_escalates_below_threshold() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    let soon = iso_utc(START + 86_400.0);
    let later = iso_utc(START + 3.0 * 86_400.0);
    let latest = iso_utc(START + 5.0 * 86_400.0);
    served(
        &host,
        &[
            ("1", usage7(50.0, 20.0, Some(&soon))),
            ("2", usage7(10.0, 10.0, Some(&later))),
            ("3", usage7(10.0, 10.0, Some(&latest))),
        ],
    );
    let mut engine = h.engine_on(&host, with(|s| s.strategy = AutoStrategy::ConsumeFirst));
    for _ in 0..4 {
        assert_eq!(engine.tick(), TickOutcome::NoAction);
        h.clock.advance(60.0);
    }
    assert_eq!(host.counts(), counts(&[("1", 2), ("2", 1), ("3", 1)]));
    let all: HashSet<String> = ["1", "2", "3"].iter().map(|s| s.to_string()).collect();
    assert!(!host.fetch_sets.borrow().contains(&all));
}

#[test]
fn adaptive_scheduler_test_engine_repairs_legacy_reset_parked_active_plan() {
    let h = scheduler(1);
    let host = StoreHost::real(&h.host, h.clock.clock());
    let reset = START + 86_400.0;
    served(&host, &[("1", usage_reset(100.0, &iso_utc(reset)))]);
    let mut engine = h.engine_on(&host, AutoSwitchSettings::default());
    engine.tick();
    host.set_plan("1", reset, 300.0);
    h.clock.advance(400.0);
    engine.tick();
    assert_eq!(host.count("1"), 2);
    assert!(host.entry("1").next_poll_at.unwrap() < reset);
}

#[test]
fn adaptive_scheduler_test_consume_first_stale_target_holds_then_switches() {
    let h = scheduler(3);
    let host = StoreHost::real(&h.host, h.clock.clock());
    let soon = iso_utc(START + 86_400.0);
    let later = iso_utc(START + 3.0 * 86_400.0);
    let latest = iso_utc(START + 5.0 * 86_400.0);
    served(
        &host,
        &[
            ("1", usage7(50.0, 20.0, Some(&soon))),
            ("2", usage7(10.0, 10.0, Some(&later))),
            ("3", usage7(10.0, 10.0, Some(&latest))),
        ],
    );
    let mut engine = h.engine_on(&host, with(|s| s.strategy = AutoStrategy::ConsumeFirst));
    engine.tick();
    h.clock.advance(60.0);
    engine.tick();
    assert_eq!(host.counts(), counts(&[("1", 1), ("2", 1), ("3", 1)]));
    host.store
        .as_ref()
        .unwrap()
        .record(
            &[("2".into(), FetchRecord::error("http-429", Some(600.0)))],
            &host.identities(),
            None,
            &HashMap::new(),
        )
        .unwrap();
    h.clock.advance(181.0);
    h.events.borrow_mut().clear();
    host.set("1", usage7(50.0, 20.0, Some(&latest)));
    assert_eq!(engine.tick(), TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
    assert!(h.reasons().contains(&"stale-usage"));
    assert_eq!(host.count("2"), 1);
    h.events.borrow_mut().clear();
    h.clock.advance(700.0);
    assert_eq!(engine.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    let (trigger, ..) = h.switch().unwrap();
    assert_eq!(trigger, Trigger::ConsumeFirst);
}
