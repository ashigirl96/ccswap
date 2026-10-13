//! Usage as `list` and `status` render it: the human lines and the
//! schema-v1 JSON projection (upstream `switcher.py` list rendering and
//! `json_output.py`).

use serde_json::{Map, Value, json};

use crate::accounts::display_tag;
use crate::listing::ListReport;
use crate::poll_policy::SERVE_TTL_S;
use crate::presentation::{SCHEMA_VERSION, Style};
use crate::usage::{account_headroom, error_note, fresh_reset_strings, now_secs, truthy};
use crate::usage_store::{DecisionValue, UsageEntry};

/// Python `str()` of a JSON scalar as the human view prints it.
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn pct_cell(pct: Option<&Value>) -> String {
    format!("{:>3.0}%", pct.and_then(Value::as_f64).unwrap_or(0.0))
}

/// `f"{x:,.2f}"`.
fn money(x: f64) -> String {
    let fixed = format!("{:.2}", x.abs());
    let (int, frac) = fixed.split_once('.').expect("two decimals");
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    format!("{sign}{grouped}.{frac}")
}

/// `just now`, `Nm ago`, `Nh ago` or `Nd ago` since an epoch time.
pub fn format_age(epoch_s: f64) -> String {
    let elapsed = now_secs().floor() as i64 - ((epoch_s * 1000.0) as i64).div_euclid(1000);
    if elapsed < 60 {
        "just now".into()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

/// `label: body` rows for one measurement, labels padded to the widest.
pub fn format_usage_lines(usage: &Value) -> Vec<String> {
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(spend)) = usage.get("spend").filter(|v| truthy(v)) {
        let num = |k: &str| spend.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let amounts = format!("${} / ${}", money(num("used")), money(num("limit")));
        let pct = pct_cell(spend.get("pct"));
        rows.push((
            "$$".into(),
            match fresh_reset_strings(spend) {
                Some((_, clock)) => format!("{pct}   resets {clock:<12}  {amounts}"),
                None => format!("{pct}   {amounts}"),
            },
        ));
    }
    let window_body = |w: &Map<String, Value>, marker: &str| {
        let pct = pct_cell(w.get("pct"));
        match fresh_reset_strings(w) {
            Some((countdown, clock)) => {
                format!("{pct}   resets {clock:<12}  in {countdown}{marker}")
            }
            None => format!("{pct}{marker}"),
        }
    };
    for (label, key) in [("5h", "five_hour"), ("7d", "seven_day")] {
        if let Some(Value::Object(w)) = usage.get(key).filter(|v| truthy(v)) {
            rows.push((label.into(), window_body(w, "")));
        }
    }
    if let Some(Value::Array(scoped)) = usage.get("scoped") {
        for w in scoped.iter().filter_map(Value::as_object) {
            let maxed = w
                .get("pct")
                .and_then(Value::as_f64)
                .is_some_and(|p| p >= 100.0);
            let name = w.get("name").map(py_str).unwrap_or_default();
            rows.push((name, window_body(w, if maxed { "  (!)" } else { "" })));
        }
    }
    let width = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0)
        + 1;
    rows.into_iter()
        .map(|(label, body)| format!("{:<width$} {body}", format!("{label}:")))
        .collect()
}

/// `last seen 53% used · 12m ago`.
pub fn last_seen_note(entry: &UsageEntry) -> Option<String> {
    let fetched_at = entry.fetched_at?;
    let headroom = account_headroom(entry.last_good.as_ref(), &[])?;
    Some(format!(
        "last seen {:.0}% used · {}",
        100.0 - headroom,
        format_age(fetched_at)
    ))
}

/// Styled usage lines (without indent) for one account.
pub fn usage_entry_lines(entry: &UsageEntry, style: &Style) -> Vec<String> {
    if let Some(sentinel) = entry.sentinel {
        let mut out = vec![style.dimmed(sentinel.note())];
        if let Some(seen) = last_seen_note(entry)
            && sentinel != crate::usage::Sentinel::ApiKey
        {
            out.push(format!("{} {}", style.dimmed("└"), style.muted(&seen)));
        }
        return out;
    }
    if let Some(last_good) = &entry.last_good {
        let mut lines = format_usage_lines(last_good);
        if let (Some(last), Some(age), Some(fetched_at)) =
            (lines.last_mut(), entry.age_s, entry.fetched_at)
            && age > SERVE_TTL_S
        {
            last.push_str(&format!(" · {}", format_age(fetched_at)));
        }
        let n = lines.len();
        return lines
            .iter()
            .enumerate()
            .map(|(j, line)| {
                let connector = if j == n - 1 { "└" } else { "├" };
                format!("{} {}", style.dimmed(connector), style.muted(line))
            })
            .collect();
    }
    let mut detail = "usage unavailable".to_string();
    if let Some(err) = entry.last_error.as_deref().filter(|e| !e.is_empty()) {
        detail.push_str(&format!(" ({})", error_note(err).unwrap_or(err)));
    }
    vec![style.dimmed(&detail)]
}

fn window_to_json(w: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("pct".into(), w.get("pct").cloned().unwrap_or(Value::Null));
    if let Some(r) = w.get("resets_at") {
        out.insert("resetsAt".into(), r.clone());
    }
    if let Some((countdown, clock)) = fresh_reset_strings(w) {
        out.insert("countdown".into(), countdown.into());
        out.insert("clock".into(), clock.into());
    }
    Value::Object(out)
}

/// The camelCase projection of a measurement.
pub fn usage_to_json(usage: &Value) -> Value {
    let mut out = Map::new();
    let empty = Map::new();
    let window = |key: &str| usage.get(key).map(|w| w.as_object().unwrap_or(&empty));
    if let Some(w) = window("five_hour") {
        out.insert("fiveHour".into(), window_to_json(w));
    }
    if let Some(w) = window("seven_day") {
        out.insert("sevenDay".into(), window_to_json(w));
    }
    if let Some(spend) = window("spend") {
        let mut s = Map::new();
        for key in ["used", "limit", "pct", "currency"] {
            s.insert(key.into(), spend.get(key).cloned().unwrap_or(Value::Null));
        }
        if let Some(r) = spend.get("resets_at") {
            s.insert("resetsAt".into(), r.clone());
        }
        if let Some((countdown, clock)) = fresh_reset_strings(spend) {
            s.insert("countdown".into(), countdown.into());
            s.insert("clock".into(), clock.into());
        }
        out.insert("spend".into(), Value::Object(s));
    }
    if let Some(Value::Array(scoped)) = usage.get("scoped") {
        let windows = scoped
            .iter()
            .map(|w| {
                let w = w.as_object().unwrap_or(&empty);
                let mut v = window_to_json(w);
                v["name"] = w.get("name").cloned().unwrap_or(Value::Null);
                v
            })
            .collect();
        out.insert("scoped".into(), Value::Array(windows));
    }
    Value::Object(out)
}

/// `datetime.fromtimestamp(t, utc).isoformat(timespec="seconds")` with `Z`.
fn iso_z(epoch_s: f64) -> String {
    let micros = (epoch_s * 1e6).round() as i64;
    chrono::DateTime::from_timestamp(micros.div_euclid(1_000_000), 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// Python `round(x, 1)`.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// `usageStatus`, `usage`, `[alias]`, `[disabled]` and the freshness
/// fields of the decision-grade value (stale last-good goes to
/// `lastGood*`).
pub fn insert_usage_fields(
    row: &mut Map<String, Value>,
    entry: &UsageEntry,
    alias: &str,
    disabled: bool,
) {
    let (status, usage) = match entry.decision_value() {
        DecisionValue::Usage(u) => ("ok", Some(usage_to_json(u))),
        DecisionValue::Sentinel(s) => (s.json_status(), None),
        DecisionValue::Unavailable => ("unavailable", None),
    };
    row.insert("usageStatus".into(), status.into());
    row.insert("usage".into(), usage.clone().unwrap_or(Value::Null));
    if !alias.is_empty() {
        row.insert("alias".into(), alias.into());
    }
    if disabled {
        row.insert("disabled".into(), true.into());
    }
    let Some(fetched_at) = entry.fetched_at else {
        return;
    };
    if usage.is_some() {
        row.insert("usageFetchedAt".into(), iso_z(fetched_at).into());
        if let Some(age) = entry.age_s {
            row.insert("usageAgeSeconds".into(), json!(round1(age)));
        }
    } else if let Some(last_good) = entry.last_good.as_ref().filter(|v| v.is_object()) {
        row.insert("lastGoodUsage".into(), usage_to_json(last_good));
        row.insert("lastGoodFetchedAt".into(), iso_z(fetched_at).into());
        if let Some(age) = entry.age_s {
            row.insert("lastGoodAgeSeconds".into(), json!(round1(age)));
        }
    }
}

pub fn list_lines(report: &ListReport, style: &Style) -> Vec<String> {
    let ListReport::Accounts(rows) = report else {
        return Vec::new();
    };
    let mut lines = vec![style.bolded("Accounts:")];
    for (i, row) in rows.iter().enumerate() {
        let info = &row.info;
        let label = if info.alias.is_empty() {
            info.email.clone()
        } else {
            format!("{} ({})", style.accent(&info.alias), info.email)
        };
        let mut markers = String::new();
        if info.is_active {
            markers.push_str(&format!(" {}", style.bold_accent("(active)")));
        }
        if row.disabled {
            markers.push_str(&format!(" {}", style.muted("(disabled)")));
        }
        lines.push(format!(
            "  {}: {label} {}{markers}",
            py_str(&info.number),
            style.muted(&format!("[{}]", display_tag(&info.org_name)))
        ));
        lines.extend(
            usage_entry_lines(&row.usage, style)
                .into_iter()
                .map(|l| format!("     {l}")),
        );
        if i + 1 < rows.len() {
            lines.push(String::new());
        }
    }
    lines
}

pub fn list_json(report: &ListReport) -> Value {
    let rows = match report {
        ListReport::NoStore => Vec::new(),
        ListReport::Accounts(rows) => rows
            .iter()
            .map(|row| {
                let info = &row.info;
                let mut out = Map::new();
                out.insert("number".into(), info.number.clone());
                out.insert("email".into(), info.email.as_str().into());
                out.insert("organizationName".into(), info.org_name.as_str().into());
                out.insert("organizationUuid".into(), info.org_uuid.as_str().into());
                out.insert("isOrganization".into(), (!info.org_uuid.is_empty()).into());
                out.insert("active".into(), info.is_active.into());
                insert_usage_fields(&mut out, &row.usage, &info.alias, row.disabled);
                Value::Object(out)
            })
            .collect(),
    };
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "activeAccountNumber": report.active_number().cloned().unwrap_or(Value::Null),
        "accounts": rows,
    })
}

#[cfg(test)]
mod tests {
    //! Ported from upstream test_switcher.py::TestFormatUsageLines and
    //! test_json_output.py::TestJsonHelpers.
    use super::*;

    #[test]
    fn rows_pad_labels_to_the_widest_and_flag_maxed_models() {
        let lines = format_usage_lines(&json!({
            "spend": {"used": 1234.5, "limit": 3000.0, "pct": 41.2, "currency": "USD"},
            "five_hour": {"pct": 0.0},
            "seven_day": {"pct": 100, "clock": "Jan 2 03:00", "countdown": "2d"},
            "scoped": [{"name": "Fable", "pct": 100.0}],
        }));
        assert_eq!(
            lines,
            [
                "$$:     41%   $1,234.50 / $3,000.00",
                "5h:      0%",
                "7d:    100%   resets Jan 2 03:00   in 2d",
                "Fable: 100%  (!)",
            ]
        );
    }

    #[test]
    fn money_groups_thousands() {
        assert_eq!(money(0.0), "0.00");
        assert_eq!(money(1234567.891), "1,234,567.89");
        assert_eq!(money(999.999), "1,000.00");
    }

    #[test]
    fn json_projection_maps_keys_and_puts_scoped_name_last() {
        let out = usage_to_json(&json!({
            "five_hour": {"pct": 7.0},
            "seven_day": {"pct": 62.0, "countdown": "17h 0m", "clock": "15:59"},
            "spend": {"used": 1.0, "limit": 10.0, "pct": 10.0, "currency": "USD"},
            "scoped": [{"name": "Fable", "pct": 100.0, "resets_at": "not-a-date", "clock": "x"}],
        }));
        assert_eq!(
            pyjson_compact(&out),
            r#"{"fiveHour":{"pct":7.0},"sevenDay":{"pct":62.0,"countdown":"17h 0m","clock":"15:59"},"spend":{"used":1.0,"limit":10.0,"pct":10.0,"currency":"USD"},"scoped":[{"pct":100.0,"resetsAt":"not-a-date","countdown":"?","clock":"x","name":"Fable"}]}"#
        );
    }

    fn pyjson_compact(v: &Value) -> String {
        serde_json::to_string(v).unwrap()
    }

    #[test]
    fn freshness_fields_round_age_and_truncate_the_timestamp() {
        let entry = UsageEntry {
            last_good: Some(json!({"five_hour": {"pct": 25.0}})),
            fetched_at: Some(1_700_000_000.75),
            age_s: Some(4000.04),
            ..Default::default()
        };
        let mut row = Map::new();
        insert_usage_fields(&mut row, &entry, "", false);
        assert_eq!(
            Value::Object(row),
            json!({"usageStatus": "unavailable", "usage": null,
                   "lastGoodUsage": {"fiveHour": {"pct": 25.0}},
                   "lastGoodFetchedAt": "2023-11-14T22:13:20Z", "lastGoodAgeSeconds": 4000.0})
        );
    }
}
