//! Weekly usage pace (upstream `pace.py`): whether a weekly window is
//! being used faster than the week is passing, evaluated at `fetchedAt`.

use serde_json::{Map, Value};

use crate::usage::parse_iso;

pub const WEEKLY_PERIOD_S: f64 = 7.0 * 86400.0;
/// Right after a reset almost any usage reads as far ahead.
pub const SUPPRESS_AFTER_RESET_S: f64 = 24.0 * 3600.0;
pub const AHEAD_THRESHOLD_PCT: f64 = 15.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    pub expected_pct: f64,
    pub actual_pct: f64,
    pub elapsed_s: f64,
    pub period_s: f64,
    pub ahead: bool,
}

/// Python `isinstance(x, (int, float))`, bools included.
fn py_number(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        other => other.as_f64(),
    }
}

pub fn compute_pace(window: Option<&Map<String, Value>>, fetched_at: Option<f64>) -> Option<Pace> {
    let window = window?;
    let fetched_at = fetched_at?;
    let pct = py_number(window.get("pct")?)?;
    let next_reset = parse_iso(window.get("resets_at")?.as_str()?)?.timestamp()?;
    let remaining = (next_reset - fetched_at).rem_euclid(WEEKLY_PERIOD_S);
    let elapsed = if remaining == 0.0 {
        0.0
    } else {
        WEEKLY_PERIOD_S - remaining
    };
    if elapsed < SUPPRESS_AFTER_RESET_S {
        return None;
    }
    let expected_pct = (elapsed / WEEKLY_PERIOD_S * 100.0).min(100.0);
    Some(Pace {
        expected_pct,
        actual_pct: pct,
        elapsed_s: elapsed,
        period_s: WEEKLY_PERIOD_S,
        ahead: pct - expected_pct >= AHEAD_THRESHOLD_PCT,
    })
}

/// Linear ETA for 100%; JSON-only because real usage is bursty.
pub fn projected_exhaustion_ts(pace: &Pace, fetched_at: f64) -> Option<f64> {
    if pace.elapsed_s <= 0.0 || pace.actual_pct <= 0.0 {
        return None;
    }
    let rate = pace.actual_pct / pace.elapsed_s;
    let remaining = 100.0 - pace.actual_pct;
    if remaining <= 0.0 {
        return Some(fetched_at);
    }
    Some(fetched_at + remaining / rate)
}

pub fn will_last_to_reset(pace: &Pace) -> Option<bool> {
    if pace.actual_pct <= 0.0 {
        return Some(true);
    }
    if pace.elapsed_s <= 0.0 {
        return None;
    }
    let rate = pace.actual_pct / pace.elapsed_s;
    Some(pace.actual_pct + rate * (pace.period_s - pace.elapsed_s) <= 100.0)
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_pace.py.
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1_700_000_000.0;
    const DAY: f64 = 86400.0;
    const WEEK: f64 = WEEKLY_PERIOD_S;

    fn iso(ts: f64) -> String {
        let micros = (ts * 1e6).round() as i64;
        chrono::DateTime::from_timestamp_micros(micros)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.f+00:00")
            .to_string()
    }

    fn window(pct: f64, resets_at: Option<f64>) -> Map<String, Value> {
        let mut w = Map::new();
        w.insert("pct".into(), json!(pct));
        if let Some(ts) = resets_at {
            w.insert("resets_at".into(), iso(ts).into());
        }
        w
    }

    fn pace_of(pct: f64, resets_at: f64) -> Option<Pace> {
        compute_pace(Some(&window(pct, Some(resets_at))), Some(NOW))
    }

    #[test]
    fn compute_pace_elapsed_one_day_into_the_week() {
        let p = pace_of(20.0, NOW + 6.0 * DAY).unwrap();
        assert_eq!(p.elapsed_s, DAY);
        assert_eq!(p.expected_pct, DAY / WEEK * 100.0);
    }

    #[test]
    fn compute_pace_elapsed_right_at_reset_boundary_is_suppressed() {
        assert_eq!(pace_of(5.0, NOW + WEEK), None);
    }

    #[test]
    fn compute_pace_elapsed_stale_resets_at_multiple_cycles_in_the_past_still_resolves() {
        assert_eq!(
            pace_of(20.0, NOW - 2.0 * WEEK - DAY).unwrap().elapsed_s,
            DAY
        );
    }

    #[test]
    fn compute_pace_elapsed_missing_fields_return_none() {
        assert_eq!(compute_pace(None, Some(NOW)), None);
        assert_eq!(compute_pace(Some(&window(10.0, None)), Some(NOW)), None);
        let no_pct = json!({"resets_at": iso(NOW + DAY)});
        assert_eq!(compute_pace(no_pct.as_object(), Some(NOW)), None);
        assert_eq!(
            compute_pace(Some(&window(10.0, Some(NOW + DAY))), None),
            None
        );
        let bad = json!({"pct": 10.0, "resets_at": "not-a-date"});
        assert_eq!(compute_pace(bad.as_object(), Some(NOW)), None);
    }

    #[test]
    fn suppression_window_just_inside_is_none() {
        let elapsed = SUPPRESS_AFTER_RESET_S - 1.0;
        assert_eq!(pace_of(50.0, NOW + WEEK - elapsed), None);
    }

    #[test]
    fn suppression_window_just_outside_is_not_none() {
        let elapsed = SUPPRESS_AFTER_RESET_S;
        assert_eq!(
            pace_of(50.0, NOW + WEEK - elapsed).unwrap().elapsed_s,
            elapsed
        );
    }

    #[test]
    fn ahead_threshold_meaningfully_ahead_flags_true() {
        assert!(pace_of(50.0, NOW + 6.0 * DAY).unwrap().ahead);
    }

    #[test]
    fn ahead_threshold_within_threshold_flags_false_but_still_returns() {
        assert!(!pace_of(20.0, NOW + 6.0 * DAY).unwrap().ahead);
    }

    #[test]
    fn ahead_threshold_behind_pace_flags_false() {
        assert!(!pace_of(5.0, NOW + 6.0 * DAY).unwrap().ahead);
    }

    #[test]
    fn projected_exhaustion_linear_projection() {
        let p = pace_of(50.0, NOW + 6.0 * DAY).unwrap();
        assert_eq!(projected_exhaustion_ts(&p, NOW), Some(NOW + DAY));
    }

    #[test]
    fn projected_exhaustion_already_at_or_over_100_returns_fetched_at() {
        let p = pace_of(120.0, NOW + 6.0 * DAY).unwrap();
        assert_eq!(projected_exhaustion_ts(&p, NOW), Some(NOW));
    }

    #[test]
    fn projected_exhaustion_zero_usage_has_no_projection() {
        let p = pace_of(0.0, NOW + 6.0 * DAY).unwrap();
        assert_eq!(projected_exhaustion_ts(&p, NOW), None);
    }

    #[test]
    fn will_last_to_reset_by_rate() {
        for (pct, lasts) in [(10.0, true), (50.0, false), (0.0, true), (12.0, true)] {
            let p = pace_of(pct, NOW + 6.0 * DAY).unwrap();
            assert_eq!(will_last_to_reset(&p), Some(lasts), "pct {pct}");
        }
    }

    #[test]
    fn will_last_flips_exactly_at_expected_pct() {
        for days in [1.0, 2.5, 4.0, 6.0] {
            let resets_at = NOW + (7.0 - days) * DAY;
            let expected = days * DAY / WEEK * 100.0;
            let over = pace_of(expected + 0.5, resets_at).unwrap();
            let under = pace_of(expected - 0.5, resets_at).unwrap();
            assert_eq!(will_last_to_reset(&over), Some(false));
            assert_eq!(will_last_to_reset(&under), Some(true));
        }
    }

    #[test]
    fn slightly_ahead_reads_wont_last_with_no_marker() {
        let p = pace_of(25.0, NOW + 6.0 * DAY).unwrap();
        assert!(!p.ahead);
        assert_eq!(will_last_to_reset(&p), Some(false));
    }
}
