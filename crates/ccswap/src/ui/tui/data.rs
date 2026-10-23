//! Display helpers (upstream `tui/data.py`).

use ccswap_core::pace;
use ccswap_core::poll_policy::SERVE_TTL_S;
use ccswap_core::usage::{self, Sentinel};
use chrono::DateTime;
use serde_json::Value;

pub fn sentinel_label(sentinel: Sentinel) -> &'static str {
    sentinel.note()
}

fn resets_at_ts(window: Option<&Value>) -> Option<f64> {
    usage::parse_reset_ts(window?.as_object()?.get("resets_at"))
}

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

pub fn ahead_of_pace(window: &Value, fetched_at: Option<f64>) -> bool {
    pace::compute_pace(window.as_object(), fetched_at).is_some_and(|p| p.ahead)
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::TestFormatting.
    use super::*;
    use crate::ui::tui::testing::iso_in;
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
}
