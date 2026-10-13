//! When to poll the usage endpoint next (upstream `poll_policy.py`). The
//! endpoint allows roughly 30 requests per trailing hour per identity, so
//! every surface shares one adaptive plan per account.

use serde_json::Value;

use crate::usage::{account_headroom, parse_reset_ts, relevant_windows};

pub const SERVE_TTL_S: f64 = 180.0;
pub const MIN_INTERVAL_S: f64 = 180.0;
pub const URGENT_INTERVAL_S: f64 = 60.0;
pub const ACTIVE_MAX_INTERVAL_S: f64 = 300.0;
pub const CANDIDATE_DEFAULT_INTERVAL_S: f64 = 300.0;
pub const CANDIDATE_MAX_INTERVAL_S: f64 = 600.0;
pub const EXHAUSTED_INTERVAL_S: f64 = 600.0;
pub const MOVEMENT_DELTA_PCT: f64 = 1.0;
pub const JITTER_FRAC: f64 = 0.1;
pub const EDGE_BACKOFF_S: f64 = 300.0;
pub const POST_429_MIN_INTERVAL_S: f64 = 360.0;
pub const RECENT_429_WINDOW_S: f64 = 3600.0;
pub const POST_429_BACKOFF_MULT: f64 = 1.5;
pub const POST_429_MAX_INTERVAL_S: f64 = 1800.0;
pub const ESCALATION_MARGIN_PCT: f64 = 15.0;
pub const RESET_SLACK_S: f64 = 60.0;

pub fn binding_pct(usage: Option<&Value>, models: &[String]) -> Option<f64> {
    account_headroom(usage, models).map(|h| 100.0 - h)
}

/// When the last of the at-limit windows resets.
pub fn limiting_reset_ts(usage: Option<&Value>, models: &[String]) -> Option<f64> {
    relevant_windows(usage, models)
        .iter()
        .filter(|w| w.pct >= 100.0)
        .filter_map(|w| parse_reset_ts(w.resets_at.as_ref()))
        .reduce(f64::max)
}

/// The next window reset after `now`, at any utilization.
pub fn earliest_future_reset_ts(usage: Option<&Value>, now: f64, models: &[String]) -> Option<f64> {
    relevant_windows(usage, models)
        .iter()
        .filter_map(|w| parse_reset_ts(w.resets_at.as_ref()))
        .filter(|ts| *ts > now)
        .reduce(f64::min)
}

pub struct PlanInputs<'a> {
    pub prev_interval_s: Option<f64>,
    pub prev_usage: Option<&'a Value>,
    pub new_usage: Option<&'a Value>,
    pub is_active: bool,
    pub threshold: f64,
    pub models: &'a [String],
    pub recent_429: bool,
    pub now: f64,
}

/// `(next_poll_at, interval_s)` after a successful fetch; `rng` yields
/// `[0, 1)` and only jitters `next_poll_at`.
pub fn plan_after_fetch(p: &PlanInputs, rng: &mut dyn FnMut() -> f64) -> (f64, f64) {
    let default = if p.is_active {
        MIN_INTERVAL_S
    } else {
        CANDIDATE_DEFAULT_INTERVAL_S
    };
    let ceiling = if p.is_active {
        ACTIVE_MAX_INTERVAL_S
    } else {
        CANDIDATE_MAX_INTERVAL_S
    };
    let base = p.prev_interval_s.filter(|i| *i != 0.0).unwrap_or(default);
    let prev_pct = binding_pct(p.prev_usage, p.models);
    let new_pct = binding_pct(p.new_usage, p.models);
    let (mut interval, moving) = match (prev_pct, new_pct) {
        (Some(prev), Some(new)) if (new - prev).abs() >= MOVEMENT_DELTA_PCT => {
            (MIN_INTERVAL_S.max(base / 2.0), true)
        }
        (Some(_), Some(_)) => (ceiling.min(MIN_INTERVAL_S.max(base * 1.5)), false),
        _ => (default, false),
    };
    if p.is_active
        && moving
        && !p.recent_429
        && new_pct.is_some_and(|n| n >= p.threshold - ESCALATION_MARGIN_PCT)
    {
        interval = URGENT_INTERVAL_S;
    }
    if p.recent_429 {
        let increased = (base * POST_429_BACKOFF_MULT).max(POST_429_MIN_INTERVAL_S);
        interval = POST_429_MAX_INTERVAL_S.min(interval.max(increased));
    }
    let headroom = account_headroom(p.new_usage, p.models);
    let exhausted = headroom.is_some_and(|h| h <= 0.0);
    if exhausted {
        interval = interval.max(EXHAUSTED_INTERVAL_S);
    }
    let mut next = p.now + interval * (1.0 + JITTER_FRAC * (2.0 * rng() - 1.0));
    if exhausted {
        if let Some(reset) = limiting_reset_ts(p.new_usage, p.models).filter(|r| *r > p.now) {
            next = next.min(reset + RESET_SLACK_S);
        }
    } else if let Some(reset) = earliest_future_reset_ts(p.new_usage, p.now, p.models) {
        next = next.min(reset + RESET_SLACK_S);
    }
    (next, interval)
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_poll_policy.py.
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_000_000.0;

    fn half() -> f64 {
        0.5
    }

    fn usage(pct: f64, resets_at: Option<&str>) -> Value {
        let mut five = json!({"pct": pct});
        if let Some(r) = resets_at {
            five["resets_at"] = r.into();
        }
        json!({"five_hour": five, "seven_day": {"pct": 0.0}})
    }

    fn iso(ts: f64) -> String {
        chrono::DateTime::from_timestamp(ts as i64, 0)
            .unwrap()
            .to_rfc3339()
    }

    fn plan(
        prev_interval: Option<f64>,
        prev: Option<&Value>,
        new: Option<&Value>,
        active: bool,
        recent_429: bool,
    ) -> (f64, f64) {
        plan_after_fetch(
            &PlanInputs {
                prev_interval_s: prev_interval,
                prev_usage: prev,
                new_usage: new,
                is_active: active,
                threshold: 90.0,
                models: &[],
                recent_429,
                now: NOW,
            },
            &mut half,
        )
    }

    #[test]
    fn unknown_utilization_uses_the_default() {
        assert_eq!(
            plan(None, None, Some(&usage(10.0, None)), true, false).1,
            180.0
        );
        assert_eq!(
            plan(None, None, Some(&usage(10.0, None)), false, false).1,
            300.0
        );
        assert_eq!(
            plan(
                Some(0.0),
                Some(&usage(10.0, None)),
                Some(&usage(10.0, None)),
                false,
                false
            )
            .1,
            450.0
        );
    }

    #[test]
    fn movement_halves_and_stillness_backs_off_to_the_ceiling() {
        let (a, b) = (usage(10.0, None), usage(20.0, None));
        assert_eq!(plan(Some(400.0), Some(&a), Some(&b), false, false).1, 200.0);
        assert_eq!(plan(Some(200.0), Some(&a), Some(&b), false, false).1, 180.0);
        assert_eq!(plan(Some(400.0), Some(&a), Some(&a), false, false).1, 600.0);
        assert_eq!(plan(Some(250.0), Some(&a), Some(&a), true, false).1, 300.0);
        assert_eq!(plan(Some(60.0), Some(&a), Some(&a), true, false).1, 180.0);
    }

    #[test]
    fn active_moving_inside_the_band_goes_urgent_unless_rate_limited() {
        let (a, b) = (usage(70.0, None), usage(76.0, None));
        assert_eq!(plan(Some(180.0), Some(&a), Some(&b), true, false).1, 60.0);
        assert_eq!(plan(Some(180.0), Some(&a), Some(&b), false, false).1, 180.0);
        let below = usage(74.0, None);
        assert_eq!(
            plan(Some(180.0), Some(&a), Some(&below), true, false).1,
            180.0
        );
        assert_eq!(plan(Some(180.0), Some(&a), Some(&b), true, true).1, 360.0);
    }

    #[test]
    fn post_429_grows_multiplicatively_to_its_ceiling() {
        let a = usage(10.0, None);
        assert_eq!(plan(Some(600.0), Some(&a), Some(&a), false, true).1, 900.0);
        assert_eq!(
            plan(Some(1500.0), Some(&a), Some(&a), false, true).1,
            1800.0
        );
    }

    #[test]
    fn next_poll_is_capped_at_the_next_reset_plus_slack() {
        let soon = usage(10.0, Some(&iso(NOW + 100.0)));
        assert_eq!(plan(None, None, Some(&soon), true, false).0, NOW + 160.0);
        let past = usage(10.0, Some(&iso(NOW - 100.0)));
        assert_eq!(plan(None, None, Some(&past), true, false).0, NOW + 180.0);
    }

    #[test]
    fn exhausted_keeps_a_slow_poll_capped_at_its_future_reset() {
        let far = usage(100.0, Some(&iso(NOW + 86400.0)));
        assert_eq!(
            plan(None, None, Some(&far), true, false),
            (NOW + 600.0, 600.0)
        );
        let near = usage(100.0, Some(&iso(NOW + 100.0)));
        assert_eq!(plan(None, None, Some(&near), true, false).0, NOW + 160.0);
        let gone = usage(100.0, Some(&iso(NOW - 100.0)));
        assert_eq!(plan(None, None, Some(&gone), true, false).0, NOW + 600.0);
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        for r in [0.0, 0.999_999] {
            let (next, interval) = plan_after_fetch(
                &PlanInputs {
                    prev_interval_s: None,
                    prev_usage: None,
                    new_usage: None,
                    is_active: false,
                    threshold: 90.0,
                    models: &[],
                    recent_429: false,
                    now: NOW,
                },
                &mut || r,
            );
            assert_eq!(interval, 300.0);
            assert!((NOW + 270.0..=NOW + 330.0).contains(&next));
        }
    }

    #[test]
    fn budget_invariants() {
        const { assert!(3600.0 / MIN_INTERVAL_S <= 20.0) };
        const { assert!(URGENT_INTERVAL_S < MIN_INTERVAL_S) };
        const { assert!(POST_429_MIN_INTERVAL_S > MIN_INTERVAL_S) };
        const { assert!(SERVE_TTL_S == MIN_INTERVAL_S) };
    }
}
