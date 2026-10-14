//! The files the desktop widget shares with the menu bar. The widget runs
//! sandboxed and only renders, so every number it shows is computed here.
//! They live in a directory of their own because the widget's sandbox
//! exception covers exactly that directory, away from the credentials.

use std::path::Path;

use serde_json::{Value, json};

use super::format::{
    DisplayUsage, MenuAccount, MenuSnapshot, local_part, named_pct, pct_of, resets_at_ts,
    rolled_weekly_window, scoped_windows,
};

pub const DIRNAME: &str = "widget";
pub const FEED_FILENAME: &str = "feed.json";
pub const REQUEST_FILENAME: &str = "request.json";
pub const FEED_VERSION: i64 = 1;
const MIN_STALE_AFTER_S: i64 = 300;
/// A request older than this was made while the menu bar was down; acting
/// on it later would surprise.
const REQUEST_MAX_AGE_S: f64 = 60.0;

pub fn feed(snapshot: &MenuSnapshot, threshold: i64, refresh_interval: i64, now: f64) -> Value {
    json!({
        "v": FEED_VERSION,
        "written_at": now,
        "stale_after": (refresh_interval * 3).max(MIN_STALE_AFTER_S),
        "threshold": threshold,
        "accounts": snapshot
            .accounts
            .iter()
            .map(|a| account(a, now))
            .collect::<Vec<_>>(),
    })
}

fn account(a: &MenuAccount, now: f64) -> Value {
    let label = if a.alias.is_empty() {
        local_part(&a.email)
    } else {
        a.alias.clone()
    };
    let (note, windows) = match &a.display {
        DisplayUsage::Measured(usage) => (Value::Null, windows(usage, now)),
        DisplayUsage::Note(note) => (note.clone().into(), Vec::new()),
        DisplayUsage::Unavailable => ("usage unavailable".into(), Vec::new()),
    };
    json!({
        "number": a.number,
        "label": label,
        "active": a.is_active,
        "disabled": a.disabled,
        "note": note,
        "windows": windows,
    })
}

fn windows(usage: &Value, now: f64) -> Vec<Value> {
    let session = usage.get("five_hour").cloned();
    let weekly = rolled_weekly_window(usage.get("seven_day"), now);
    let mut out: Vec<Value> = [("Current session", session), ("All models", weekly)]
        .into_iter()
        .filter_map(|(name, w)| window(name, w.as_ref()?))
        .collect();
    for scoped in scoped_windows(usage) {
        let Some(rolled) = rolled_weekly_window(Some(scoped), now) else {
            continue;
        };
        if let Some((name, _)) = rolled.as_object().and_then(named_pct) {
            out.extend(window(name, &rolled));
        }
    }
    out
}

fn window(name: &str, w: &Value) -> Option<Value> {
    let pct = pct_of(Some(w))?;
    let resets_at = resets_at_ts(Some(w));
    Some(json!({
        "name": name,
        "pct": pct,
        "resets_at": resets_at.is_finite().then_some(resets_at),
    }))
}

/// Consumes the widget's pending switch request; the account number when it
/// is fresh and well-formed.
pub fn take_request(path: &Path, now: f64) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    let request: Value = serde_json::from_str(&text).ok()?;
    let at = request.get("at")?.as_f64()?;
    let number = request.get("switch_to")?.as_str()?;
    let fresh = (now - at).abs() <= REQUEST_MAX_AGE_S;
    let valid = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    (fresh && valid).then(|| number.to_string())
}

#[cfg(test)]
mod tests;
