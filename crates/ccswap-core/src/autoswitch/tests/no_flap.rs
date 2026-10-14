//! Upstream `TestHorizonAxisDoesNotFlap`: the headroom axis's ratio margin,
//! the spent fallback, and the no-return bar with its release legs.

use super::*;
use crate::autoswitch::rank::{left_account_recovered, no_return_account, rank_candidates};

fn at(h: &Harness, seconds: f64) -> String {
    iso_utc(h.clock.now() + seconds)
}

/// Upstream `_days_out`: anchored on first use per `hours`, so a fixed
/// reset draws nearer as the clock advances.
struct Anchors(RefCell<HashMap<u64, String>>);

impl Anchors {
    fn new() -> Self {
        Anchors(RefCell::new(HashMap::new()))
    }

    fn days_out(&self, h: &Harness, hours: f64) -> String {
        self.0
            .borrow_mut()
            .entry(hours.to_bits())
            .or_insert_with(|| at(h, hours * 3600.0))
            .clone()
    }
}

fn ur(pct: f64, reset: &str) -> Option<Value> {
    usage_reset(pct, reset)
}

fn consume_first_settings() -> AutoSwitchSettings {
    with(|s| s.strategy = AutoStrategy::ConsumeFirst)
}

fn read_state(e: &AutoSwitchEngine) -> State {
    e.state().read()
}

fn mutate(e: &AutoSwitchEngine, f: impl FnOnce(&mut State)) {
    e.state().mutate(f).unwrap();
}

fn state_of(v: Value) -> State {
    v.as_object().unwrap().clone()
}

fn bar_decisions(usage: &[(&str, Option<Value>)], headroom: &[(&str, Option<f64>)]) -> Decisions {
    Decisions {
        usage: usage
            .iter()
            .map(|(n, v)| {
                (
                    n.to_string(),
                    v.clone().map_or(UsageValue::Unknown, UsageValue::Usage),
                )
            })
            .collect(),
        headroom: headroom.iter().map(|(n, h)| (n.to_string(), *h)).collect(),
    }
}

fn bar<'a>(
    state: &'a State,
    decisions: &'a Decisions,
    active_headroom: Option<f64>,
    now: f64,
    current: &'a str,
) -> BarInput<'a> {
    BarInput {
        state,
        decisions,
        active_headroom,
        threshold: 90.0,
        models: &[],
        now,
        current,
    }
}

fn no_return(
    trigger: Trigger,
    state: &State,
    headroom: &[(&str, Option<f64>)],
    active: Option<f64>,
) -> Option<String> {
    let d = bar_decisions(&[], headroom);
    no_return_account(trigger, true, &bar(state, &d, active, START, ""))
}

fn recovered(
    state: &State,
    usage: &[(&str, Option<Value>)],
    headroom: &[(&str, Option<f64>)],
    active: Option<f64>,
    now: f64,
    current: &str,
) -> bool {
    let d = bar_decisions(usage, headroom);
    left_account_recovered(&bar(state, &d, active, now, current))
}

fn moves(seen: &[Option<String>]) -> Vec<Option<String>> {
    seen.iter()
        .enumerate()
        .filter(|(i, n)| *i == 0 || **n != seen[i - 1])
        .map(|(_, n)| n.clone())
        .collect()
}

/// Three ticks with account 1 unreadable fail over onto account 2.
fn fail_over_to_two(h: &Harness, e: &mut AutoSwitchEngine, peer_reset: impl Fn() -> String) {
    let mut outcome = TickOutcome::NoAction;
    for _ in 0..3 {
        outcome = h.tick(e, &[("1", None), ("2", ur(4.0, &peer_reset()))]);
        h.clock.advance(60.0);
    }
    assert_eq!(outcome, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

fn fleet(n: u32) -> Harness {
    let h = Harness::bare();
    for (num, email) in [
        (1, "a@example.com"),
        (2, "b@example.com"),
        (3, "c@example.com"),
    ]
    .into_iter()
    .take(n as usize)
    {
        h.host.seed(num, email);
    }
    h.host.make_live("a@example.com");
    h
}

#[test]
fn horizon_axis_does_not_flap_test_a_fixed_reset_crosses_into_the_horizon_as_the_clock_advances() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out1 = h.tick(
        &mut e,
        &[
            ("1", ur(95.0, &a.days_out(&h, 400.0))),
            ("2", ur(94.0, &a.days_out(&h, 5.0))),
        ],
    );
    assert_ne!(out1, TickOutcome::Switched);
    h.clock.advance(90.0 * 60.0);
    let out2 = h.tick(
        &mut e,
        &[
            ("1", ur(95.0, &a.days_out(&h, 400.0))),
            ("2", ur(94.0, &a.days_out(&h, 5.0))),
        ],
    );
    assert_eq!(out2, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_one_point_of_headroom_does_not_move() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(95.0, &a.days_out(&h, 109.0))),
            ("2", ur(94.0, &a.days_out(&h, 80.0))),
            ("3", ur(99.0, &a.days_out(&h, 50.0))),
        ],
    );
    assert_eq!(h.active(), s("1"));
    assert_ne!(out, TickOutcome::Switched);
}

#[test]
fn horizon_axis_does_not_flap_test_the_return_leg_is_blocked_too() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(96.0, &a.days_out(&h, 109.0))),
            ("2", ur(95.0, &a.days_out(&h, 80.0))),
            ("3", ur(99.0, &a.days_out(&h, 50.0))),
        ],
    );
    assert_eq!(h.active(), s("1"));
    assert_ne!(out, TickOutcome::Switched);
}

#[test]
fn horizon_axis_does_not_flap_test_a_pair_straddling_the_horizon_does_not_ping_pong() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let r_far = a.days_out(&h, 109.0);
    let r_near = a.days_out(&h, 3.5);
    let mut seen = Vec::new();
    for _ in 0..6 {
        h.tick(&mut e, &[("1", ur(92.0, &r_far)), ("2", ur(97.0, &r_near))]);
        seen.push(h.active());
        h.clock.advance(301.0);
    }
    assert!(
        seen.iter().all(|n| *n == seen[0]),
        "cross-axis oscillation: {seen:?}"
    );
}

#[test]
fn horizon_axis_does_not_flap_test_the_spent_fallback_needs_a_meaningfully_sooner_reset() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(97.5, &a.days_out(&h, 500.02))),
            ("2", ur(96.0, &a.days_out(&h, 500.0))),
        ],
    );
    assert_eq!(h.active(), s("1"));
    assert_ne!(out, TickOutcome::Switched);
}

#[test]
fn horizon_axis_does_not_flap_test_the_tier_byte_puts_a_returning_peer_ahead_of_a_distant_one() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(99.0, &a.days_out(&h, 300.0))),
            ("2", ur(98.5, &a.days_out(&h, 1.0))),
            ("3", ur(91.0, &a.days_out(&h, 400.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_fallback_breaks_a_reset_tie_by_headroom() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let same = a.days_out(&h, 10.0);
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(97.0, &a.days_out(&h, 300.0))),
            ("2", ur(97.0, &same)),
            ("3", ur(96.95, &same)),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn horizon_axis_does_not_flap_test_past_the_horizon_headroom_decides_before_the_reset() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(99.0, &a.days_out(&h, 20.0))),
            ("2", ur(98.0, &a.days_out(&h, 10.0))),
            ("3", ur(96.0, &a.days_out(&h, 20.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn horizon_axis_does_not_flap_test_a_burn_walk_settles_instead_of_oscillating() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let mut pct: HashMap<String, f64> = [("1".into(), 96.0), ("2".into(), 92.0)].into();
    let mut seen = Vec::new();
    for _ in 0..24 {
        h.tick(
            &mut e,
            &[
                ("1", ur(pct["1"], &a.days_out(&h, 20.0))),
                ("2", ur(pct["2"], &a.days_out(&h, 80.0))),
            ],
        );
        let active = h.active();
        seen.push(active.clone());
        let p = pct.get_mut(active.as_deref().unwrap()).unwrap();
        *p = (*p + 0.25).min(99.95);
        h.clock.advance(301.0);
    }
    let m = moves(&seen);
    assert!(m.len() <= 2, "move sequence {m:?}");
    assert!(
        seen[seen.len() - 4..]
            .iter()
            .all(|n| *n == seen[seen.len() - 1]),
        "active trace {seen:?}"
    );
}

#[test]
fn horizon_axis_does_not_flap_test_the_no_return_filter_does_not_block_the_at_limit_escape() {
    let h = Harness::three();
    let mut e = h.default_engine();
    mutate(&e, |st| {
        st.insert("lastSwitchFrom".into(), json!("2"));
    });
    let out = h.tick(&mut e, &[("1", usage(100.0)), ("2", usage(0.0))]);
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_a_burn_walk_never_returns_to_what_it_left() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let mut pct: HashMap<String, f64> = [("1".into(), 92.0), ("2".into(), 92.0)].into();
    let mut seen = Vec::new();
    for _ in 0..60 {
        h.tick(
            &mut e,
            &[
                ("1", ur(pct["1"], &a.days_out(&h, 500.0))),
                ("2", ur(pct["2"], &a.days_out(&h, 400.0))),
            ],
        );
        let active = h.active();
        seen.push(active.clone());
        let p = pct.get_mut(active.as_deref().unwrap()).unwrap();
        *p = (*p + 0.5).min(99.95);
        h.clock.advance(301.0);
    }
    let tail = &seen[seen.len() - 8..];
    assert!(
        tail.iter().all(|n| *n == tail[0]),
        "move sequence {:?}",
        moves(&seen)
    );
}

#[test]
fn horizon_axis_does_not_flap_test_a_proactive_move_does_not_lock_out_the_next_one() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..20 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(0.0, &a.days_out(&h, 400.0))),
                ("2", ur(97.0, &a.days_out(&h, 500.0))),
            ],
        ));
        h.clock.advance(1801.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_an_ordinary_departure_does_not_stall_on_a_weekly_bound_peer() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.engine(consume_first_settings());
    let d20 = a.days_out(&h, 20.0);
    let d10 = a.days_out(&h, 10.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", usage7(30.0, 30.0, Some(&d20))),
                ("2", usage7(50.0, 50.0, Some(&d10))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for active_pct in [50.0, 80.0, 90.0, 95.0, 98.0, 99.5, 100.0] {
        let d400 = a.days_out(&h, 400.0);
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", usage7(30.0, 30.0, Some(&d20))),
                ("2", usage7(active_pct, active_pct, Some(&d400))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert_ne!(outcomes[0], TickOutcome::Switched, "{outcomes:?}");
    assert!(
        outcomes[..outcomes.len() - 1].contains(&TickOutcome::Switched),
        "{outcomes:?}"
    );
}

#[test]
fn horizon_axis_does_not_flap_test_a_filtered_candidate_does_not_forge_an_all_exhausted_claim() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(100.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    h.events.borrow_mut().clear();
    h.tick(
        &mut e,
        &[
            ("1", ur(0.0, &a.days_out(&h, 400.0))),
            ("2", ur(97.0, &a.days_out(&h, 500.0))),
            ("3", ur(100.0, &a.days_out(&h, 300.0))),
        ],
    );
    assert!(!h.has("all-exhausted"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_does_not_hide_the_account_from_the_census() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(100.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    h.events.borrow_mut().clear();
    h.tick(
        &mut e,
        &[
            ("1", ur(85.0, &a.days_out(&h, 400.0))),
            ("2", ur(90.0, &a.days_out(&h, 500.0))),
            ("3", ur(100.0, &a.days_out(&h, 300.0))),
        ],
    );
    assert!(!h.has("all-exhausted"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_lifts_when_it_would_leave_nothing() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.engine(consume_first_settings());
    let d500 = a.days_out(&h, 500.0);
    let d400 = a.days_out(&h, 400.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", usage7(95.0, 95.0, Some(&d500))),
                ("2", usage7(5.0, 5.0, Some(&d400))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..20 {
        let d10 = a.days_out(&h, 10.0);
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", usage7(0.0, 0.0, Some(&d10))),
                ("2", usage7(30.0, 30.0, Some(&d500))),
            ],
        ));
        h.clock.advance(1801.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_never_applies_to_an_escape() {
    let state = state_of(json!({"lastSwitchFrom": 1}));
    let headroom = [("1", Some(15.0)), ("2", Some(10.0)), ("3", Some(1.0))];
    for trigger in [Trigger::AtLimit, Trigger::Failover] {
        for active in [Some(10.0), None] {
            assert_eq!(
                no_return(trigger, &state, &headroom, active),
                None,
                "trigger={trigger:?} active_headroom={active:?}"
            );
        }
    }
    assert_eq!(
        no_return(Trigger::Proactive, &state, &headroom, Some(10.0)).as_deref(),
        Some("1")
    );
}

fn bar_only_holds_while_the_engine_is_where_it_landed(
    landed: Option<Value>,
    live: &str,
    expect: &str,
) {
    let h = fleet(3);
    let mut e = h.default_engine();
    let back: HashMap<&str, String> = [("1", 1800.0), ("2", 7200.0), ("3", 3600.0)]
        .into_iter()
        .map(|(n, secs)| (n, at(&h, secs)))
        .collect();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &back["1"])),
                ("2", ur(10.0, &back["2"])),
                ("3", ur(92.0, &back["3"])),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    let state = read_state(&e);
    assert_eq!(state["lastSwitchFrom"], json!(1));
    assert_eq!(state["lastSwitchTo"], json!("2"));
    mutate(&e, |st| match landed {
        None => {
            st.shift_remove("lastSwitchTo");
        }
        Some(v) => {
            st.insert("lastSwitchTo".into(), v);
        }
    });
    if live != "2" {
        h.host.make_live("c@example.com");
    }
    assert_eq!(h.active(), s(live));
    h.clock.advance(301.0);
    h.tick(
        &mut e,
        &[
            ("1", ur(99.0, &back["1"])),
            ("2", ur(99.0, &back["2"])),
            ("3", ur(99.0, &back["3"])),
        ],
    );
    assert_eq!(h.active(), s(expect), "landed / live = {live}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_only_holds_while_the_engine_is_where_it_landed_str_on_landing()
 {
    bar_only_holds_while_the_engine_is_where_it_landed(Some(json!("2")), "2", "3");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_only_holds_while_the_engine_is_where_it_landed_str_hand_switched()
 {
    bar_only_holds_while_the_engine_is_where_it_landed(Some(json!("2")), "3", "1");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_only_holds_while_the_engine_is_where_it_landed_int_on_landing()
 {
    bar_only_holds_while_the_engine_is_where_it_landed(Some(json!(2)), "2", "3");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_only_holds_while_the_engine_is_where_it_landed_int_hand_switched()
 {
    bar_only_holds_while_the_engine_is_where_it_landed(Some(json!(2)), "3", "1");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_only_holds_while_the_engine_is_where_it_landed_absent() {
    bar_only_holds_while_the_engine_is_where_it_landed(None, "3", "3");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_actually_removes_the_account_from_the_ranking() {
    let decisions = Decisions::new(
        vec![
            ("1".into(), UsageValue::Usage(usage(40.0).unwrap())),
            ("2".into(), UsageValue::Usage(usage(96.0).unwrap())),
            ("3".into(), UsageValue::Usage(usage(99.0).unwrap())),
        ],
        &[],
    );
    let candidates = vec!["1".to_string(), "3".to_string()];
    let rank = |no_return: Option<&str>| {
        rank_candidates(&RankInput {
            trigger: Trigger::Proactive,
            consume_first: false,
            oauth_candidates: &candidates,
            no_return,
            decisions: &decisions,
            current: "2",
            active_headroom: Some(4.0),
            threshold: 90.0,
            hysteresis_pct: 10.0,
            models: &[],
            now: START,
        })
        .ordered
    };
    assert_eq!(rank(None), ["1"]);
    assert!(rank(Some("1")).is_empty());
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_lifts_when_the_only_alternative_cannot_be_chosen() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(100.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..30 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(97.0, &a.days_out(&h, 10.0))),
                ("2", ur(98.0, &a.days_out(&h, 500.0))),
                ("3", ur(100.0, &a.days_out(&h, 300.0))),
            ],
        ));
        h.clock.advance(3601.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_lifts_for_an_alternative_the_ranking_would_reject() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(99.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..30 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.5, &a.days_out(&h, 10.0))),
                ("2", ur(98.0, &a.days_out(&h, 500.0))),
                ("3", ur(99.0, &a.days_out(&h, 300.0))),
            ],
        ));
        h.clock.advance(3601.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_an_unreadable_barred_account_does_not_crash_the_tick() {
    let state = state_of(json!({"lastSwitchFrom": 1}));
    assert_eq!(
        no_return(
            Trigger::Proactive,
            &state,
            &[("2", Some(10.0)), ("3", Some(40.0))],
            Some(10.0)
        )
        .as_deref(),
        Some("1")
    );
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_lifts_for_a_peer_returning_inside_the_horizon() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &at(&h, 3600.0))),
                ("2", ur(97.0, &a.days_out(&h, 200.0))),
            ],
        ));
        h.clock.advance(1801.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_same_fleet_moves_with_the_bar_cleared() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(99.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(301.0);
    mutate(&e, |st| {
        st.shift_remove("lastSwitchFrom");
    });
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(96.5, &a.days_out(&h, 10.0))),
                ("2", ur(98.0, &a.days_out(&h, 500.0))),
                ("3", ur(99.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_reaches_the_ranking_through_tick() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(50.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(301.0);
    assert!(
        read_state(&e)
            .get("lastSwitchFrom")
            .is_some_and(|v| !v.is_null())
    );

    let second = [
        ("1", ur(99.0, &at(&h, 1800.0))),
        ("2", ur(99.0, &at(&h, 7200.0))),
        ("3", ur(99.0, &at(&h, 3600.0))),
    ];
    assert_eq!(h.tick(&mut e, &second), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));

    h.host.make_live("b@example.com");
    h.clock.advance(301.0);
    mutate(&e, |st| {
        st.shift_remove("lastSwitchFrom");
    });
    assert_eq!(h.tick(&mut e, &second), TickOutcome::Switched);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_release_needs_the_barred_account_to_have_improved() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &a.days_out(&h, 500.0))),
                ("2", ur(92.0, &a.days_out(&h, 400.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(3612.0);
    let mut outcomes = Vec::new();
    for active_pct in [98.0, 98.2, 98.4, 99.0, 99.5] {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &a.days_out(&h, 500.0))),
                ("2", ur(active_pct, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(!outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_release_fires_on_this_same_fleet_once_the_peer_actually_improves()
 {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &a.days_out(&h, 500.0))),
                ("2", ur(92.0, &a.days_out(&h, 400.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(3612.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(0.0, &a.days_out(&h, 500.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_a_departure_at_full_quota_is_immediately_eligible() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.engine(consume_first_settings());
    let d500 = a.days_out(&h, 500.0);
    let d100 = a.days_out(&h, 100.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", usage7(0.0, 0.0, Some(&d500))),
                ("2", usage7(0.0, 0.0, Some(&d100))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    assert_eq!(read_state(&e).get("leftHeadroom"), Some(&json!(100.0)));
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", usage7(0.0, 0.0, Some(&d500))),
                ("2", usage7(90.0, 0.0, Some(&d100))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_clamp_stays_load_bearing_when_dominance_does_not_fire() {
    let state =
        state_of(json!({"lastSwitchFrom": "1", "leftHeadroom": 98.0, "leftRecoveryAt": null}));
    assert!(recovered(
        &state,
        &[("1", usage(0.0))],
        &[("1", Some(100.0))],
        Some(60.0),
        START,
        "",
    ));
    const { assert!(100.0 < 98.0 + rank::SPENT_HEADROOM_PCT) };
}

#[test]
fn horizon_axis_does_not_flap_test_the_dominance_leg_does_not_silently_read_an_unreadable_active_as_no_dominance()
 {
    let state =
        state_of(json!({"lastSwitchFrom": "2", "leftHeadroom": 40.0, "leftRecoveryAt": null}));
    let usage_rows = [("2", usage(60.0))];
    let headroom = [("2", Some(40.0))];
    assert!(recovered(
        &state,
        &usage_rows,
        &headroom,
        Some(2.0),
        START,
        "1"
    ));
    assert!(recovered(&state, &usage_rows, &headroom, None, START, "1"));
}

#[test]
fn horizon_axis_does_not_flap_test_no_return_account_does_not_re_bar_an_already_recovered_peer_when_the_active_is_unreadable()
 {
    let state =
        state_of(json!({"lastSwitchFrom": "2", "leftHeadroom": 40.0, "leftRecoveryAt": null}));
    let headroom = [("2", Some(40.0))];
    for trigger in [Trigger::Proactive, Trigger::ConsumeFirst] {
        assert_eq!(no_return(trigger, &state, &headroom, Some(2.0)), None);
        assert_eq!(
            no_return(trigger, &state, &headroom, None),
            None,
            "{trigger:?}"
        );
    }
}

#[test]
fn horizon_axis_does_not_flap_test_the_all_spent_recovery_leg_carries_its_own_hysteresis() {
    let h = fleet(2);
    let state =
        state_of(json!({"lastSwitchFrom": "1", "leftHeadroom": null, "leftRecoveryAt": null}));
    let usage_rows = [
        ("1", ur(95.0, &at(&h, 3600.0))),
        ("2", ur(98.0, &at(&h, 3660.0))),
    ];
    assert!(!recovered(
        &state,
        &usage_rows,
        &[("1", Some(5.0))],
        Some(2.0),
        h.clock.now(),
        "2",
    ));
}

#[test]
fn horizon_axis_does_not_flap_test_the_dominance_fallback_does_not_fire_below_its_own_floor() {
    let state =
        state_of(json!({"lastSwitchFrom": "2", "leftHeadroom": 40.0, "leftRecoveryAt": null}));
    assert!(!recovered(
        &state,
        &[("2", usage(95.0))],
        &[("2", Some(5.0))],
        None,
        START,
        "1",
    ));
}

#[test]
fn horizon_axis_does_not_flap_test_no_return_accounts_unreadable_active_fallback_has_a_floor_too() {
    let state = state_of(json!({"lastSwitchFrom": "2"}));
    assert_eq!(
        no_return(Trigger::Proactive, &state, &[("2", Some(5.0))], None).as_deref(),
        Some("2")
    );
}

#[test]
fn horizon_axis_does_not_flap_test_a_reset_that_crept_nearer_is_not_a_recovery() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    let depart = at(&h, 500.0 * 3600.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &depart)),
                ("2", ur(92.0, &a.days_out(&h, 400.0)))
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(3612.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &at(&h, 500.0 * 3600.0 - 3612.0 - 60.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(!outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_an_unschedulable_account_that_gained_a_reset_has_recovered() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[("1", usage(96.0)), ("2", ur(92.0, &a.days_out(&h, 400.0)))]
        ),
        TickOutcome::Switched
    );
    assert!(
        read_state(&e)
            .get("leftRecoveryAt")
            .is_none_or(Value::is_null)
    );
    h.clock.advance(3612.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &at(&h, 3600.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_a_switch_that_recorded_no_snapshot_still_releases() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    mutate(&e, |st| {
        st.insert("lastSwitchFrom".into(), json!("2"));
    });
    mutate(&e, |st| {
        st.shift_remove("leftHeadroom");
    });
    mutate(&e, |st| {
        st.shift_remove("leftRecoveryAt");
    });
    assert!(!read_state(&e).contains_key("leftHeadroom"));
    let mut outcomes = Vec::new();
    for _ in 0..20 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(97.0, &a.days_out(&h, 500.0))),
                ("2", ur(0.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(1801.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_absence_of_a_snapshot_releases_even_a_poor_or_unreadable_peer() {
    let state = state_of(json!({"lastSwitchFrom": "2"}));
    assert!(recovered(
        &state,
        &[("2", usage(96.0))],
        &[("2", Some(4.0))],
        Some(2.0),
        START,
        "",
    ));
    assert!(recovered(
        &state,
        &[("2", None)],
        &[("2", None)],
        Some(2.0),
        START,
        "",
    ));
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_departure_does_not_disarm_the_bar() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    let frozen1 = a.days_out(&h, 500.0);
    fail_over_to_two(&h, &mut e, || a.days_out(&h, 400.0));
    let state = read_state(&e);
    assert!(state.contains_key("leftHeadroom"));
    assert!(state["leftHeadroom"].is_null());
    assert!(state["leftRecoveryAt"].is_null());
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for active_pct in [98.0, 98.2, 98.4, 99.0, 99.5, 99.9] {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &frozen1)),
                ("2", ur(active_pct, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(!outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_departure_still_unreadable_does_not_crash_or_release()
{
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    fail_over_to_two(&h, &mut e, || a.days_out(&h, 400.0));
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[("1", None), ("2", ur(98.0, &a.days_out(&h, 400.0)))],
        ));
        h.clock.advance(301.0);
    }
    assert!(!outcomes.contains(&TickOutcome::Error), "{outcomes:?}");
    assert!(!outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

fn failover_then(
    h: &Harness,
    a: &Anchors,
    rows: impl Fn() -> Vec<(&'static str, Option<Value>)>,
    ticks: usize,
) -> Vec<TickOutcome> {
    let mut e = h.default_engine();
    fail_over_to_two(h, &mut e, || a.days_out(h, 400.0));
    let state = read_state(&e);
    assert!(state["leftHeadroom"].is_null() && state["leftRecoveryAt"].is_null());
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..ticks {
        outcomes.push(h.tick(&mut e, &rows()));
        h.clock.advance(301.0);
    }
    outcomes
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_departure_releases_once_the_peer_is_readable_again() {
    let h = fleet(2);
    let a = Anchors::new();
    let outcomes = failover_then(
        &h,
        &a,
        || {
            vec![
                ("1", ur(0.0, &a.days_out(&h, 10.0))),
                ("2", ur(95.0, &a.days_out(&h, 400.0))),
            ]
        },
        10,
    );
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_departure_releases_a_healthy_but_not_near_full_peer()
{
    let h = fleet(2);
    let a = Anchors::new();
    let outcomes = failover_then(
        &h,
        &a,
        || {
            vec![
                ("1", ur(30.0, &a.days_out(&h, 10.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ]
        },
        10,
    );
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_departure_releases_when_the_peer_resets_first_in_the_all_spent_regime()
 {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    fail_over_to_two(&h, &mut e, || at(&h, 400.0 * 3600.0));
    let state = read_state(&e);
    assert!(state["leftHeadroom"].is_null() && state["leftRecoveryAt"].is_null());
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..8 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(97.5, &at(&h, 300.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_recovery_leg_requires_the_actives_reset_to_be_known_not_merely_absent()
 {
    type Row = Box<dyn Fn(&Harness, &Anchors) -> Option<Value>>;
    let cases: Vec<(&str, Row, TickOutcome, &str)> = vec![
        (
            "active reset UNREPORTED",
            Box::new(|_, _| usage(98.0)),
            TickOutcome::Blocked,
            "2",
        ),
        (
            "active resets in 500h",
            Box::new(|h, a| ur(98.0, &a.days_out(h, 500.0))),
            TickOutcome::Switched,
            "1",
        ),
        (
            "active resets in 10min",
            Box::new(|h, _| ur(98.0, &at(h, 600.0))),
            TickOutcome::Blocked,
            "2",
        ),
    ];
    for (label, active_row, expected, expected_active) in cases {
        let h = fleet(2);
        let a = Anchors::new();
        let mut e = h.default_engine();
        fail_over_to_two(&h, &mut e, || at(&h, 400.0 * 3600.0));
        h.clock.advance(301.0);
        let out = h.tick(
            &mut e,
            &[
                ("1", ur(97.5, &at(&h, 400.0 * 3600.0))),
                ("2", active_row(&h, &a)),
            ],
        );
        assert_eq!((out, h.active()), (expected, s(expected_active)), "{label}");
    }
}

#[test]
fn horizon_axis_does_not_flap_test_the_isfinite_guard_must_not_hold_when_a_near_peer_is_available()
{
    type Reset = Box<dyn Fn(&Harness) -> String>;
    type ActiveRow = Box<dyn Fn(&Harness) -> Option<Value>>;
    let cases: Vec<(&str, ActiveRow, Reset, TickOutcome, &str)> = vec![
        (
            "POS",
            Box::new(|h| ur(98.0, &at(h, 400.0 * 3600.0))),
            Box::new(|h| at(h, 3000.0)),
            TickOutcome::Switched,
            "1",
        ),
        (
            "NEG",
            Box::new(|h| ur(98.0, &at(h, 400.0 * 3600.0))),
            Box::new(|h| at(h, 400.0 * 3600.0 - 60.0)),
            TickOutcome::Blocked,
            "2",
        ),
        (
            "DMG-a",
            Box::new(|_| usage(98.0)),
            Box::new(|h| at(h, 3000.0)),
            TickOutcome::Switched,
            "1",
        ),
        (
            "DMG-b",
            Box::new(|h| ur(98.0, &at(h, -3600.0))),
            Box::new(|h| at(h, 3000.0)),
            TickOutcome::Switched,
            "1",
        ),
    ];
    for (label, active_row, peer_reset, expected, expected_active) in cases {
        let h = fleet(2);
        let mut e = h.default_engine();
        fail_over_to_two(&h, &mut e, || at(&h, 400.0 * 3600.0));
        h.clock.advance(301.0);
        let out = h.tick(
            &mut e,
            &[("1", ur(96.0, &peer_reset(&h))), ("2", active_row(&h))],
        );
        assert_eq!((out, h.active()), (expected, s(expected_active)), "{label}");
    }
}

#[test]
fn horizon_axis_does_not_flap_test_left_snapshot_uses_the_ranking_now_not_a_fresh_clock_read() {
    let h = fleet(2);
    let ranking_now = START;
    let reset_at = iso_utc(ranking_now + 100.0);
    let stale_reread = ranking_now + 200.0;
    let calls = Arc::new(AtomicU64::new(0));
    let counter = calls.clone();
    // Every read up to and including the ranking's sees `ranking_now`;
    // any later read is past the active's reset.
    let clock: Clock = Arc::new(move || {
        if counter.fetch_add(1, Ordering::SeqCst) < 3 {
            ranking_now
        } else {
            stale_reread
        }
    });
    let events = h.events.clone();
    let mut e = AutoSwitchEngine::new(
        &h.host,
        AutoSwitchSettings::default(),
        Box::new(move |ev| events.borrow_mut().push(ev)),
    )
    .clock(clock);
    let out = h.tick(&mut e, &[("1", ur(95.0, &reset_at)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(
        read_state(&e).get("leftRecoveryAt"),
        Some(&json!(ranking_now + 100.0))
    );
}

#[test]
fn horizon_axis_does_not_flap_test_the_all_spent_stall_above_is_the_floor_not_the_fleet() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    fail_over_to_two(&h, &mut e, || at(&h, 400.0 * 3600.0));
    mutate(&e, |st| {
        st.shift_remove("leftHeadroom");
        st.shift_remove("leftRecoveryAt");
    });
    assert!(!read_state(&e).contains_key("leftHeadroom"));
    h.clock.advance(301.0);
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(97.5, &at(&h, 300.0))),
            ("2", ur(98.0, &a.days_out(&h, 400.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
}

#[test]
fn horizon_axis_does_not_flap_test_a_failover_hold_still_escapes_at_limit() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    fail_over_to_two(&h, &mut e, || a.days_out(&h, 400.0));
    let state = read_state(&e);
    assert!(state.contains_key("leftHeadroom"));
    assert!(state["leftHeadroom"].is_null() && state["leftRecoveryAt"].is_null());
    h.clock.advance(301.0);
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &a.days_out(&h, 500.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(!outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(0.0, &a.days_out(&h, 500.0))),
            ("2", ur(100.0, &a.days_out(&h, 400.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_release_fires_when_the_barred_account_recovered() {
    let h = fleet(2);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(96.0, &a.days_out(&h, 500.0))),
                ("2", ur(92.0, &a.days_out(&h, 400.0))),
            ]
        ),
        TickOutcome::Switched
    );
    h.clock.advance(3612.0);
    let mut outcomes = Vec::new();
    for _ in 0..10 {
        outcomes.push(h.tick(
            &mut e,
            &[
                ("1", ur(0.0, &a.days_out(&h, 500.0))),
                ("2", ur(98.0, &a.days_out(&h, 400.0))),
            ],
        ));
        h.clock.advance(301.0);
    }
    assert!(outcomes.contains(&TickOutcome::Switched), "{outcomes:?}");
}

#[test]
fn horizon_axis_does_not_flap_test_the_ratio_release_changes_where_the_engine_lands() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.default_engine();
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(92.0, &a.days_out(&h, 500.0))),
                ("2", ur(10.0, &a.days_out(&h, 400.0))),
                ("3", ur(70.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(301.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", ur(20.0, &a.days_out(&h, 500.0))),
                ("2", ur(90.0, &a.days_out(&h, 400.0))),
                ("3", ur(70.0, &a.days_out(&h, 300.0))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("1"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_bar_is_recomputed_on_the_phase_two_snapshot() {
    let h = fleet(3);
    let a = Anchors::new();
    let mut e = h.engine(consume_first_settings());
    let d500 = a.days_out(&h, 500.0);
    let d10 = a.days_out(&h, 10.0);
    let d400 = a.days_out(&h, 400.0);
    assert_eq!(
        h.tick(
            &mut e,
            &[
                ("1", usage7(20.0, 20.0, Some(&d500))),
                ("2", usage7(5.0, 5.0, Some(&d10))),
                ("3", usage7(5.0, 5.0, Some(&d400))),
            ]
        ),
        TickOutcome::Switched
    );
    assert_eq!(h.active(), s("2"));
    h.clock.advance(301.0);

    let stale = vec![
        (
            "1".to_string(),
            Val::Dict(usage7(80.0, 80.0, Some(&d10)).unwrap()),
        ),
        (
            "2".to_string(),
            Val::Dict(usage7(70.0, 70.0, Some(&d500)).unwrap()),
        ),
        (
            "3".to_string(),
            Val::Dict(usage7(60.0, 60.0, Some(&d400)).unwrap()),
        ),
    ];
    let fresh = vec![
        (
            "1".to_string(),
            Val::Dict(usage7(10.0, 10.0, Some(&d10)).unwrap()),
        ),
        (
            "2".to_string(),
            Val::Dict(usage7(85.0, 85.0, Some(&d500)).unwrap()),
        ),
        (
            "3".to_string(),
            Val::Dict(usage7(60.0, 60.0, Some(&d400)).unwrap()),
        ),
    ];
    let clock = h.clock.clone();
    h.host.serve(Box::new(move |fetch| {
        let snap = if fetch.len() >= 3 { &fresh } else { &stale };
        snap.iter()
            .map(|(n, v)| (n.clone(), entry_for(v, clock.now())))
            .collect()
    }));
    assert_eq!(e.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_fallback_never_outranks_a_real_qualifier() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(97.0, &a.days_out(&h, 500.0))),
            ("2", ur(94.0, &a.days_out(&h, 400.0))),
            ("3", ur(96.4, &a.days_out(&h, 100.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_the_fallback_ranks_by_reset_not_by_headroom() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(98.0, &a.days_out(&h, 300.0))),
            ("2", ur(98.0, &a.days_out(&h, 10.0))),
            ("3", ur(96.9, &a.days_out(&h, 50.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_a_materially_better_peer_still_wins() {
    let h = Harness::three();
    let a = Anchors::new();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(98.0, &a.days_out(&h, 109.0))),
            ("2", ur(90.0, &a.days_out(&h, 80.0))),
            ("3", ur(99.0, &a.days_out(&h, 50.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn horizon_axis_does_not_flap_test_a_minutes_away_reset_is_unaffected() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", ur(91.0, &at(&h, 7200.0))),
            ("2", ur(94.0, &at(&h, 1800.0))),
            ("3", ur(98.0, &at(&h, 480.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}
