//! Display helpers (upstream `tui/data.py`) and the weekly pace marker
//! (upstream `pace.py`).

use ccswap_core::poll_policy::SERVE_TTL_S;
use ccswap_core::usage::{self, Sentinel, parse_iso};
use chrono::DateTime;
use serde_json::Value;

pub fn sentinel_label(sentinel: Sentinel) -> &'static str {
    sentinel.note()
}

fn py_number(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        other => other.as_f64(),
    }
}

fn resets_at_ts(window: Option<&Value>) -> Option<f64> {
    usage::parse_reset_ts(window?.as_object()?.get("resets_at"))
}

/// `resets 2h 13m`, or `resets now` once elapsed.
pub fn reset_text(window: Option<&Value>, now: f64) -> Option<String> {
    let remaining = resets_at_ts(window)? - now;
    if remaining <= 0.0 {
        return Some("resets now".into());
    }
    Some(format!("resets {}", format_duration(remaining)))
}

/// The local reset time (`20:39`, `Jul 14 09:00`); `None` once elapsed.
pub fn reset_clock(window: Option<&Value>, now: f64) -> Option<String> {
    let ts = resets_at_ts(window)?;
    if ts - now <= 0.0 {
        return None;
    }
    let at = |t: f64| DateTime::from_timestamp_micros((t * 1e6) as i64);
    Some(usage::reset_clock(at(ts)?, at(now)?))
}

/// `45s`, `12m`, `2h 13m`, `3d 4h`.
pub fn format_duration(seconds: f64) -> String {
    let s = seconds as i64;
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m", s / 60);
    }
    if s < 86400 {
        let (h, m) = (s / 60 / 60, s / 60 % 60);
        return if m > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{h}h")
        };
    }
    let (d, h) = (s / 3600 / 24, s / 3600 % 24);
    if h > 0 {
        format!("{d}d {h}h")
    } else {
        format!("{d}d")
    }
}

/// `· 6m ago`; `None` while inside the serve TTL.
pub fn format_age(age_s: Option<f64>) -> Option<String> {
    let age = age_s.filter(|a| *a >= SERVE_TTL_S)?;
    Some(format!("· {} ago", format_duration(age)))
}

const WEEKLY_PERIOD_S: f64 = 7.0 * 86400.0;
const SUPPRESS_AFTER_RESET_S: f64 = 24.0 * 3600.0;
const AHEAD_THRESHOLD_PCT: f64 = 15.0;

/// Whether a weekly window has used meaningfully more than the elapsed
/// share of its cycle, measured at `fetched_at`.
pub fn ahead_of_pace(window: &Value, fetched_at: Option<f64>) -> bool {
    let (Some(window), Some(fetched_at)) = (window.as_object(), fetched_at) else {
        return false;
    };
    let Some(pct) = window.get("pct").and_then(py_number) else {
        return false;
    };
    let Some(next_reset) = window
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_iso)
        .and_then(|t| t.timestamp())
    else {
        return false;
    };
    let remaining = (next_reset - fetched_at).rem_euclid(WEEKLY_PERIOD_S);
    let elapsed = if remaining == 0.0 {
        0.0
    } else {
        WEEKLY_PERIOD_S - remaining
    };
    if elapsed < SUPPRESS_AFTER_RESET_S {
        return false;
    }
    let expected = (elapsed / WEEKLY_PERIOD_S * 100.0).min(100.0);
    pct - expected >= AHEAD_THRESHOLD_PCT
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::TestFormatting.
    use super::*;
    use crate::tui::testing::iso_in;
    use ccswap_core::usage::now_secs;
    use serde_json::json;

    #[test]
    fn format_duration_forms() {
        assert_eq!(format_duration(42.0), "42s");
        assert_eq!(format_duration(180.0), "3m");
        assert_eq!(format_duration(7980.0), "2h 13m");
        assert_eq!(format_duration(3600.0 * 26.0), "1d 2h");
        assert_eq!(format_duration(7200.0), "2h");
        assert_eq!(format_duration(86400.0 * 3.0), "3d");
    }

    #[test]
    fn format_age_fresh_is_silent() {
        assert_eq!(format_age(Some(3.0)), None);
        assert_eq!(format_age(Some(120.0)), None);
        assert_eq!(format_age(None), None);
        assert_eq!(format_age(Some(400.0)).as_deref(), Some("· 6m ago"));
    }

    #[test]
    fn sentinel_labels_match_list() {
        assert_eq!(
            sentinel_label(Sentinel::TokenExpired),
            "token expired — refresh deferred this pass; retries automatically"
        );
        assert_eq!(sentinel_label(Sentinel::ApiKey), "API key (no quota)");
    }

    #[test]
    fn reset_clock_same_day_is_bare_and_later_is_dated() {
        let now = now_secs();
        let seven = json!({"resets_at": iso_in(86400.0 * 3.0)});
        let clock7 = reset_clock(Some(&seven), now).unwrap();
        let months = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        assert!(months.iter().any(|m| clock7.contains(m)));
        let soon = json!({"resets_at": iso_in(60.0)});
        let local = chrono::Local::now();
        if (local + chrono::Duration::seconds(90)).date_naive() == local.date_naive() {
            assert_eq!(
                reset_clock(Some(&soon), now).unwrap().matches(':').count(),
                1
            );
        }
    }

    #[test]
    fn reset_clock_unknown_or_elapsed_is_none() {
        let now = now_secs();
        assert_eq!(reset_clock(None, now), None);
        assert_eq!(reset_clock(Some(&json!({"pct": 5.0})), now), None);
        assert_eq!(
            reset_clock(Some(&json!({"resets_at": "garbage"})), now),
            None
        );
        let elapsed = json!({"resets_at": iso_in(-60.0)});
        assert_eq!(reset_clock(Some(&elapsed), now), None);
        assert_eq!(
            reset_text(Some(&elapsed), now).as_deref(),
            Some("resets now")
        );
    }

    #[test]
    fn pace_marker_needs_a_day_into_the_week_and_a_fifteen_point_gap() {
        let now = now_secs();
        let one_day_in = json!({"pct": 50.0, "resets_at": iso_in(86400.0 * 6.0)});
        assert!(ahead_of_pace(&one_day_in, Some(now)));
        assert!(!ahead_of_pace(&one_day_in, None));
        let just_reset = json!({"pct": 50.0, "resets_at": iso_in(86400.0 * 6.5)});
        assert!(!ahead_of_pace(&just_reset, Some(now)));
        let on_pace = json!({"pct": 20.0, "resets_at": iso_in(86400.0 * 6.0)});
        assert!(!ahead_of_pace(&on_pace, Some(now)));
    }
}
