//! Ported from upstream tests/test_poll_policy.py; names keep upstream's
//! `Class::test` names.

use serde_json::{Value, json};

use super::*;

const NOW: f64 = 1_000_000.0;

fn usage(pct: f64) -> Value {
    json!({"five_hour": {"pct": pct}, "seven_day": {"pct": 0.0}})
}

fn usage_reset(pct: f64, reset_ts: f64) -> Value {
    let iso = crate::store::format_timestamp(reset_ts as i64);
    json!({"five_hour": {"pct": pct, "resets_at": iso}, "seven_day": {"pct": 0.0}})
}

/// Upstream `_plan`: a candidate fetched at 10%, threshold 90, rng at the
/// midpoint (jitter factor exactly 1.0).
struct Plan {
    prev_interval_s: Option<f64>,
    prev_usage: Option<Value>,
    new_usage: Option<Value>,
    is_active: bool,
    recent_429: bool,
    rng: f64,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            prev_interval_s: None,
            prev_usage: None,
            new_usage: Some(usage(10.0)),
            is_active: false,
            recent_429: false,
            rng: 0.5,
        }
    }
}

impl Plan {
    fn run(&self) -> (f64, f64) {
        let r = self.rng;
        plan_after_fetch(
            &PlanInputs {
                prev_interval_s: self.prev_interval_s,
                prev_usage: self.prev_usage.as_ref(),
                new_usage: self.new_usage.as_ref(),
                is_active: self.is_active,
                threshold: 90.0,
                models: &[],
                recent_429: self.recent_429,
                now: NOW,
            },
            &mut || r,
        )
    }

    fn interval(&self) -> f64 {
        self.run().1
    }
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-6 * b.abs().max(1.0)
}

#[test]
fn interval_adaptation_test_first_fetch_uses_defaults() {
    let active = Plan {
        is_active: true,
        ..Default::default()
    };
    assert_eq!(active.interval(), MIN_INTERVAL_S);
    assert_eq!(Plan::default().interval(), CANDIDATE_DEFAULT_INTERVAL_S);
}

#[test]
fn interval_adaptation_test_unmoved_decays_toward_the_ceiling() {
    let unmoved = |prev: f64, is_active: bool| Plan {
        prev_interval_s: Some(prev),
        prev_usage: Some(usage(10.0)),
        is_active,
        ..Default::default()
    };
    assert_eq!(unmoved(300.0, false).interval(), 450.0);
    assert_eq!(unmoved(500.0, false).interval(), CANDIDATE_MAX_INTERVAL_S);
    assert_eq!(unmoved(250.0, true).interval(), ACTIVE_MAX_INTERVAL_S);
}

#[test]
fn interval_adaptation_test_movement_halves_floored_at_min() {
    let moved = |prev: f64| Plan {
        prev_interval_s: Some(prev),
        prev_usage: Some(usage(10.0)),
        new_usage: Some(usage(15.0)),
        ..Default::default()
    };
    assert_eq!(moved(600.0).interval(), 300.0);
    assert_eq!(moved(200.0).interval(), MIN_INTERVAL_S);
}

#[test]
fn interval_adaptation_test_sub_delta_wiggle_is_not_movement() {
    let plan = Plan {
        prev_interval_s: Some(300.0),
        prev_usage: Some(usage(10.0)),
        new_usage: Some(usage(10.5)),
        ..Default::default()
    };
    assert_eq!(plan.interval(), 450.0);
}

#[test]
fn interval_adaptation_test_unknown_pct_uses_the_default() {
    let plan = Plan {
        prev_interval_s: Some(600.0),
        new_usage: None,
        ..Default::default()
    };
    assert_eq!(plan.interval(), CANDIDATE_DEFAULT_INTERVAL_S);
}

#[test]
fn interval_adaptation_a_stored_zero_interval_counts_as_missing() {
    let plan = Plan {
        prev_interval_s: Some(0.0),
        prev_usage: Some(usage(10.0)),
        ..Default::default()
    };
    assert_eq!(plan.interval(), 450.0);
}

/// Active, moving, inside the 75..90 band.
fn urgent() -> Plan {
    Plan {
        prev_interval_s: Some(MIN_INTERVAL_S),
        prev_usage: Some(usage(78.0)),
        new_usage: Some(usage(82.0)),
        is_active: true,
        ..Default::default()
    }
}

#[test]
fn urgent_mode_test_active_moving_in_band_goes_urgent() {
    assert_eq!(urgent().interval(), URGENT_INTERVAL_S);
}

#[test]
fn urgent_mode_test_candidate_never_goes_urgent() {
    let plan = Plan {
        is_active: false,
        ..urgent()
    };
    assert_eq!(plan.interval(), MIN_INTERVAL_S);
}

#[test]
fn urgent_mode_test_no_movement_no_urgency() {
    let plan = Plan {
        new_usage: Some(usage(78.0)),
        ..urgent()
    };
    assert!(plan.interval() > URGENT_INTERVAL_S);
}

#[test]
fn urgent_mode_test_below_the_band_no_urgency() {
    let plan = Plan {
        prev_usage: Some(usage(40.0)),
        new_usage: Some(usage(50.0)),
        ..urgent()
    };
    assert_eq!(plan.interval(), MIN_INTERVAL_S);
}

#[test]
fn urgent_mode_test_recent_429_suppresses_urgency() {
    let plan = Plan {
        recent_429: true,
        ..urgent()
    };
    assert_eq!(plan.interval(), POST_429_MIN_INTERVAL_S);
}

#[test]
fn urgent_mode_test_urgent_then_unmoved_snaps_back_to_the_floor() {
    let plan = Plan {
        prev_interval_s: Some(URGENT_INTERVAL_S),
        new_usage: Some(usage(78.0)),
        ..urgent()
    };
    assert_eq!(plan.interval(), MIN_INTERVAL_S);
}

#[test]
fn post429_floor_test_recent_429_floors_the_cadence() {
    let plan = Plan {
        recent_429: true,
        prev_usage: Some(usage(10.0)),
        ..Default::default()
    };
    assert!(plan.interval() >= POST_429_MIN_INTERVAL_S);
}

#[test]
fn post429_floor_test_slower_learned_cadence_survives_the_floor() {
    let interval = Plan {
        recent_429: true,
        prev_interval_s: Some(590.0),
        prev_usage: Some(usage(10.0)),
        ..Default::default()
    }
    .interval();
    assert!(approx(interval, 590.0 * POST_429_BACKOFF_MULT));
    assert!(interval > POST_429_MIN_INTERVAL_S);
}

fn contended(prev: Option<f64>, recent_429: bool) -> Plan {
    Plan {
        recent_429,
        prev_interval_s: prev,
        prev_usage: Some(usage(10.0)),
        new_usage: Some(usage(10.0)),
        ..Default::default()
    }
}

#[test]
fn post429_aimd_test_recent_429_multiplicatively_increases_from_prev() {
    let interval = contended(Some(POST_429_MIN_INTERVAL_S), true).interval();
    assert!(interval > POST_429_MIN_INTERVAL_S);
    assert!(approx(
        interval,
        POST_429_MIN_INTERVAL_S * POST_429_BACKOFF_MULT
    ));
}

#[test]
fn post429_aimd_test_recent_429_ceiling_exceeds_normal_candidate_max() {
    const { assert!(POST_429_MAX_INTERVAL_S > CANDIDATE_MAX_INTERVAL_S) };
    assert_eq!(
        contended(Some(POST_429_MAX_INTERVAL_S), true).interval(),
        POST_429_MAX_INTERVAL_S
    );
}

#[test]
fn post429_aimd_test_no_429_uses_normal_ceiling() {
    assert_eq!(
        contended(Some(590.0), false).interval(),
        CANDIDATE_MAX_INTERVAL_S
    );
}

/// Twelve successful polls of an unmoving, still-contended token.
fn converge_trajectory(recent_429: bool) -> Vec<f64> {
    let mut prev = None;
    (0..12)
        .map(|_| {
            let interval = contended(prev, recent_429).interval();
            prev = Some(interval);
            interval
        })
        .collect()
}

fn max(traj: &[f64]) -> f64 {
    traj.iter().copied().fold(f64::MIN, f64::max)
}

#[test]
fn post429_aimd_test_sustained_429_grows_the_interval_to_the_wide_ceiling() {
    let traj = converge_trajectory(true);
    assert_eq!(*traj.last().unwrap(), POST_429_MAX_INTERVAL_S);
    assert!(traj.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!(max(&traj), POST_429_MAX_INTERVAL_S);
    for w in traj.windows(2) {
        if w[1] < POST_429_MAX_INTERVAL_S {
            assert!(approx(w[1], w[0] * POST_429_BACKOFF_MULT));
        }
    }
}

#[test]
fn post429_aimd_test_without_recency_the_interval_is_capped_at_the_narrow_ceiling() {
    assert_eq!(max(&converge_trajectory(false)), CANDIDATE_MAX_INTERVAL_S);
    const { assert!(CANDIDATE_MAX_INTERVAL_S < POST_429_MAX_INTERVAL_S) };
}

fn at(pct: f64, reset_ts: f64) -> Plan {
    Plan {
        new_usage: Some(usage_reset(pct, reset_ts)),
        ..Default::default()
    }
}

#[test]
fn reset_capping_test_poll_never_scheduled_past_a_future_reset() {
    let reset_ts = NOW + 90.0;
    let (next, interval) = at(40.0, reset_ts).run();
    assert!(approx(next, reset_ts + RESET_SLACK_S));
    assert_eq!(interval, CANDIDATE_DEFAULT_INTERVAL_S);
}

#[test]
fn reset_capping_test_at_limit_keeps_bounded_polling_before_distant_reset() {
    let reset_ts = NOW + 7_200.0;
    let (next, interval) = at(100.0, reset_ts).run();
    assert_eq!(interval, EXHAUSTED_INTERVAL_S);
    assert!(approx(next, NOW + interval));
    assert!(next < reset_ts);
}

#[test]
fn reset_capping_test_at_limit_poll_is_pulled_to_an_imminent_reset() {
    let reset_ts = NOW + 90.0;
    let (next, interval) = at(100.0, reset_ts).run();
    assert_eq!(interval, EXHAUSTED_INTERVAL_S);
    assert!(approx(next, reset_ts + RESET_SLACK_S));
}

#[test]
fn reset_capping_test_at_limit_ignores_non_future_reset() {
    for reset_ts in [NOW - 90.0, NOW] {
        let (next, interval) = at(100.0, reset_ts).run();
        assert_eq!(interval, EXHAUSTED_INTERVAL_S);
        assert!(approx(next, NOW + interval), "{reset_ts}");
    }
}

#[test]
fn reset_capping_test_active_at_limit_uses_same_bounded_recovery_probe() {
    let plan = Plan {
        is_active: true,
        ..at(100.0, NOW + 7_200.0)
    };
    let (next, interval) = plan.run();
    assert_eq!(interval, EXHAUSTED_INTERVAL_S);
    assert!(approx(next, NOW + interval));
}

#[test]
fn jitter_test_jitter_bounds() {
    let early = Plan {
        rng: 0.0,
        ..Default::default()
    };
    let late = Plan {
        rng: 1.0,
        ..Default::default()
    };
    let interval = CANDIDATE_DEFAULT_INTERVAL_S;
    assert!(approx(early.run().0, NOW + interval * 0.9));
    assert!(approx(late.run().0, NOW + interval * 1.1));
}

#[test]
fn budget_invariants_test_sustained_floor_stays_under_the_hourly_cap() {
    const { assert!(MIN_INTERVAL_S >= 180.0) };
    const { assert!(SERVE_TTL_S >= 180.0) };
}

#[test]
fn budget_invariants_test_edge_backoff_probes_slower_than_capacity_frees() {
    const { assert!(EDGE_BACKOFF_S >= 300.0) };
}

#[test]
fn budget_invariants_test_post_429_floor_covers_the_saturation_horizon() {
    const { assert!(RECENT_429_WINDOW_S >= 3600.0) };
    const { assert!(POST_429_MIN_INTERVAL_S >= MIN_INTERVAL_S) };
}

#[test]
fn budget_invariants_test_urgent_episode_alone_fits_inside_the_window_cap() {
    const { assert!(ESCALATION_MARGIN_PCT / MOVEMENT_DELTA_PCT < 27.0) };
}
