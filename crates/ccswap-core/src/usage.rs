//! Usage measurements in the normalised shape every provider's fetch produces
//! (upstream `oauth.py`'s), plus the states that stand in for one.

use crate::pyjson::py_str;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use serde_json::{Map, Value};

/// Usage states that stand in for a measurement. Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentinel {
    NoCredentials,
    TokenExpired,
    ApiKey,
    KeychainUnavailable,
    ReloginRequired,
    ForeignCredential,
}

impl Sentinel {
    /// The `usageStatus` value in `--json` output.
    pub fn json_status(self) -> &'static str {
        match self {
            Sentinel::NoCredentials => "no_credentials",
            Sentinel::TokenExpired => "token_expired",
            Sentinel::ApiKey => "api_key",
            Sentinel::KeychainUnavailable => "keychain_unavailable",
            Sentinel::ReloginRequired => "relogin_required",
            Sentinel::ForeignCredential => "foreign_credential",
        }
    }

    /// The one-line note in human output.
    pub fn note(self) -> &'static str {
        match self {
            Sentinel::NoCredentials => "no credentials",
            Sentinel::TokenExpired => {
                "token expired — refresh deferred this pass; retries automatically"
            }
            Sentinel::ApiKey => "API key (no quota)",
            Sentinel::KeychainUnavailable => "keychain unavailable — locked or in use; try again",
            Sentinel::ReloginRequired => {
                "re-login needed — refresh token dead; log in with Claude Code, then run: ccswap add"
            }
            Sentinel::ForeignCredential => {
                "live credential belongs to another account — a switch repairs it"
            }
        }
    }
}

/// Friendly text for fetch-error kinds that need more than their identifier.
pub fn error_note(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "store-unmirrored" => {
            "CLAUDE_SECURESTORAGE_CONFIG_DIR set — unset it or run from a normal shell"
        }
        "invalid_client" => "cswap's OAuth client was rejected — systemic, not this account",
        "consume-busy" => "another cswap surface holds the slot — retries next pass",
        "stash-unreadable" => {
            "this slot's stashed successor is unreadable — unlock the keychain or fix the file, then retry; `ccswap unclaimed` inspects it"
        }
        _ => return None,
    })
}

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python exception class names, which upstream persists as error kinds.
pub type PyError = &'static str;

/// A `datetime.fromisoformat` result: aware or naive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IsoTime {
    Aware(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
}

impl IsoTime {
    /// `datetime.timestamp()`: a naive time is read as local time.
    pub fn timestamp(self) -> Option<f64> {
        let dt = match self {
            IsoTime::Aware(dt) => dt.with_timezone(&Utc),
            IsoTime::Naive(naive) => Local
                .from_local_datetime(&naive)
                .earliest()?
                .with_timezone(&Utc),
        };
        Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_micros()) / 1e6)
    }
}

/// `datetime.fromisoformat` for the extended forms the usage API emits.
pub fn parse_iso(s: &str) -> Option<IsoTime> {
    let b = s.as_bytes();
    if b.len() < 10 || !s.is_char_boundary(10) {
        return None;
    }
    let date = parse_date(&s[..10])?;
    if b.len() == 10 {
        return Some(IsoTime::Naive(date.and_time(NaiveTime::MIN)));
    }
    let rest = s[10..].chars().skip(1).collect::<String>();
    let tz_at = rest.find(['Z', '+', '-']);
    let (time_part, tz_part) = match tz_at {
        Some(i) => (&rest[..i], Some(&rest[i..])),
        None => (rest.as_str(), None),
    };
    let time = parse_time(time_part)?;
    let naive = date.and_time(time);
    let Some(tz) = tz_part else {
        return Some(IsoTime::Naive(naive));
    };
    let offset = parse_offset(tz)?;
    Some(IsoTime::Aware(offset.from_local_datetime(&naive).single()?))
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let (y, rest) = s.split_at(4);
    let (m, d) = rest.strip_prefix('-')?.split_once('-')?;
    NaiveDate::from_ymd_opt(digits(y, 4)? as i32, digits(m, 2)?, digits(d, 2)?)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    let (clock, frac) = match s.find(['.', ',']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let mut parts = clock.split(':');
    let h = digits(parts.next()?, 2)?;
    let m = parts.next().map_or(Some(0), |p| digits(p, 2))?;
    let sec = parts.next().map_or(Some(0), |p| digits(p, 2))?;
    if parts.next().is_some() {
        return None;
    }
    let micros = match frac {
        None => 0,
        Some(f) if !f.is_empty() && f.bytes().all(|c| c.is_ascii_digit()) => {
            let mut six: String = f.chars().take(6).collect();
            while six.len() < 6 {
                six.push('0');
            }
            six.parse().ok()?
        }
        Some(_) => return None,
    };
    NaiveTime::from_hms_micro_opt(h, m, sec, micros)
}

fn parse_offset(s: &str) -> Option<FixedOffset> {
    if s == "Z" {
        return FixedOffset::east_opt(0);
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let body = &s[1..];
    let (h, m) = if let Some((h, rest)) = body.split_once(':') {
        let m = rest.split(':').next()?;
        (digits(h, 2)?, digits(m, 2)?)
    } else {
        match body.len() {
            2 => (digits(body, 2)?, 0),
            4 => (digits(&body[..2], 2)?, digits(&body[2..], 2)?),
            _ => return None,
        }
    };
    FixedOffset::east_opt(sign * (h * 3600 + m * 60) as i32)
}

/// `(countdown, clock)` for a reset time, the clock as `reset_clock` formats it.
pub fn format_reset(resets_at: &Value) -> Result<(String, String), PyError> {
    let text = resets_at.as_str().ok_or("TypeError")?;
    let reset = match parse_iso(text).ok_or("ValueError")? {
        IsoTime::Aware(dt) => dt.with_timezone(&Utc),
        IsoTime::Naive(_) => return Err("TypeError"),
    };
    let now = Utc::now();
    let remaining = (reset - now).num_microseconds().unwrap_or(i64::MAX);
    let total = (remaining / 1_000_000).max(0);
    let (days, rem) = (total / 86400, total % 86400);
    let (hours, minutes) = (rem / 3600, rem % 3600 / 60);
    let countdown = if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    };
    Ok((countdown, reset_clock(reset, now)))
}

/// Local `20:39` on the same day, else `Jul 5 08:59`.
pub fn reset_clock(reset: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let reset_local = reset.with_timezone(&Local);
    if reset_local.date_naive() == now.with_timezone(&Local).date_naive() {
        return reset_local.format("%H:%M").to_string();
    }
    format!(
        "{} {} {}",
        reset_local.format("%b"),
        reset_local.format("%-d"),
        reset_local.format("%H:%M")
    )
}

/// Countdown and clock recomputed now; the fetch-time strings when
/// `resets_at` is missing or unparseable.
pub fn fresh_reset_strings(window: &Map<String, Value>) -> Option<(String, String)> {
    if let Some(resets_at) = window.get("resets_at").filter(|v| truthy(v))
        && let Ok(cell) = format_reset(resets_at)
    {
        return Some(cell);
    }
    let clock = window.get("clock")?;
    let countdown = window.get("countdown").map_or("?".into(), py_str);
    Some((countdown, py_str(clock)))
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `isinstance(v, (int, float))`, bools included.
pub fn py_number(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        other => other.as_f64(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub label: String,
    pub pct: f64,
    pub resets_at: Option<Value>,
}

/// The 5h and 7d windows, plus the scoped windows named in `models`
/// (case-insensitive; `all` matches every one). Spend never gates.
pub fn relevant_windows(usage: Option<&Value>, models: &[String]) -> Vec<Window> {
    let Some(Value::Object(usage)) = usage else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        if let Some(Value::Object(w)) = usage.get(key)
            && let Some(pct) = w.get("pct").and_then(py_number)
        {
            windows.push(Window {
                label: label.into(),
                pct,
                resets_at: w.get("resets_at").cloned(),
            });
        }
    }
    if !models.is_empty() {
        let wanted: Vec<String> = models.iter().map(|m| m.to_lowercase()).collect();
        let match_all = wanted.iter().any(|m| m == "all");
        if let Some(Value::Array(scoped)) = usage.get("scoped") {
            for s in scoped {
                let Value::Object(s) = s else { continue };
                if let (Some(pct), Some(name)) = (
                    s.get("pct").and_then(py_number),
                    s.get("name").and_then(Value::as_str),
                ) && (match_all || wanted.contains(&name.to_lowercase()))
                {
                    windows.push(Window {
                        label: name.into(),
                        pct,
                        resets_at: s.get("resets_at").cloned(),
                    });
                }
            }
        }
    }
    windows
}

/// `100 - max(pct)` over the relevant windows; `None` when there are none.
pub fn account_headroom(usage: Option<&Value>, models: &[String]) -> Option<f64> {
    relevant_windows(usage, models)
        .iter()
        .map(|w| w.pct)
        .reduce(f64::max)
        .map(|max| 100.0 - max)
}

/// Epoch seconds of a reset string (a naive one read as local time).
pub fn parse_reset_ts(resets_at: Option<&Value>) -> Option<f64> {
    let value = resets_at.filter(|v| truthy(v))?;
    let text = match value {
        Value::String(s) => s.replace('Z', "+00:00"),
        other => other.to_string(),
    };
    parse_iso(&text)?.timestamp()
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_oauth.py (TestAccountHeadroom,
    //! TestRelevantWindows, TestFormatReset) and test_json_output.py (fresh reset strings).
    use super::*;
    use serde_json::json;

    fn iso_in(secs: i64) -> String {
        (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
    }

    #[test]
    fn headroom_is_the_binding_window_and_models_add_scoped() {
        let usage = json!({
            "five_hour": {"pct": 30.0}, "seven_day": {"pct": 55},
            "scoped": [{"name": "Fable", "pct": 90.0}],
            "spend": {"pct": 99.0},
        });
        assert_eq!(account_headroom(Some(&usage), &[]), Some(45.0));
        assert_eq!(
            account_headroom(Some(&usage), &["fable".into()]),
            Some(10.0)
        );
        assert_eq!(account_headroom(Some(&usage), &["ALL".into()]), Some(10.0));
        assert_eq!(account_headroom(Some(&usage), &["opus".into()]), Some(45.0));
        assert_eq!(account_headroom(Some(&json!({})), &[]), None);
        assert_eq!(account_headroom(None, &[]), None);
    }

    #[test]
    fn countdown_forms() {
        assert_eq!(format_reset(&iso_in(5 * 60 + 30).into()).unwrap().0, "5m");
        assert_eq!(
            format_reset(&iso_in(2 * 3600 + 30).into()).unwrap().0,
            "2h 0m"
        );
        assert_eq!(
            format_reset(&iso_in(3 * 86400 + 30).into()).unwrap().0,
            "3d 0h"
        );
        assert_eq!(format_reset(&iso_in(-600).into()).unwrap().0, "0m");
        assert_eq!(
            format_reset(&"2026-01-01T00:00:00".into()),
            Err("TypeError")
        );
        assert_eq!(format_reset(&json!(5)), Err("TypeError"));
    }

    #[test]
    fn clock_is_time_only_on_the_same_local_day_else_dated() {
        let now = Local::now();
        let same_day = now.date_naive().and_hms_opt(23, 59, 59).unwrap();
        let same = Local.from_local_datetime(&same_day).unwrap();
        if same > now + chrono::Duration::seconds(5) {
            let (_, clock) = format_reset(&same.to_rfc3339().into()).unwrap();
            assert_eq!(clock, "23:59");
        }
        let later = Local
            .from_local_datetime(
                &(now.date_naive() + chrono::Duration::days(3))
                    .and_hms_opt(8, 59, 0)
                    .unwrap(),
            )
            .unwrap();
        let (_, clock) = format_reset(&later.to_rfc3339().into()).unwrap();
        use chrono::Datelike;
        assert_eq!(
            clock,
            format!("{} {} 08:59", later.format("%b"), later.day())
        );
    }

    #[test]
    fn fresh_strings_recompute_or_fall_back() {
        let w =
            json!({"resets_at": iso_in(2 * 3600 + 30 * 60), "countdown": "17h 0m", "clock": "x"});
        assert!(
            fresh_reset_strings(w.as_object().unwrap())
                .unwrap()
                .0
                .starts_with("2h")
        );
        let w = json!({"resets_at": "not-a-date", "countdown": "17h 0m", "clock": "15:59"});
        assert_eq!(
            fresh_reset_strings(w.as_object().unwrap()),
            Some(("17h 0m".into(), "15:59".into()))
        );
        assert_eq!(
            fresh_reset_strings(json!({"pct": 1}).as_object().unwrap()),
            None
        );
    }

    #[test]
    fn iso_parsing_matches_fromisoformat_forms() {
        let ts = |s: &str| parse_reset_ts(Some(&json!(s)));
        assert_eq!(ts("1970-01-01T00:00:10Z"), Some(10.0));
        assert_eq!(ts("1970-01-01T01:00:10+01:00"), Some(10.0));
        assert_eq!(ts("1970-01-01T00:00:10.5+00:00"), Some(10.5));
        assert_eq!(ts("1970-01-01 00:00:10.123456789Z"), Some(10.123456));
        assert_eq!(ts("garbage"), None);
        assert_eq!(parse_reset_ts(Some(&json!(""))), None);
    }
}
