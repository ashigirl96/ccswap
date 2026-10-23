//! `{:.0}` matches Python's `:.0f` (ties to even); `f64::round` would not.

use std::collections::HashMap;

use ccswap_claude::snapshot::AccountsSnapshot;
use ccswap_codex::CodexListing;
use ccswap_codex::view::sentinel_note;
use ccswap_core::pace::compute_pace;
use ccswap_core::usage::parse_iso;
use ccswap_core::usage_store::UsageEntry;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};

use super::settings::MenuBarSettings;

pub const ICON: &str = "⇄";
pub const SWITCH_HISTORY_LIMIT: usize = 10;
const WEEKLY_PERIOD_S: f64 = 7.0 * 86400.0;
const SEPARATOR: &str = " · ";

#[derive(Debug, Clone, PartialEq)]
pub enum DisplayUsage {
    Note(String),
    Measured(Value),
    Unavailable,
}

impl DisplayUsage {
    pub fn from_entry(entry: &UsageEntry) -> Self {
        if let Some(sentinel) = entry.sentinel {
            return DisplayUsage::Note(sentinel.note().to_string());
        }
        match &entry.last_good {
            Some(usage) => DisplayUsage::Measured(usage.clone()),
            None => DisplayUsage::Unavailable,
        }
    }

    fn measured(&self) -> Option<&Value> {
        match self {
            DisplayUsage::Measured(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuAccount {
    pub number: String,
    pub email: String,
    pub is_active: bool,
    pub display: DisplayUsage,
    pub last_good: Option<Value>,
    pub alias: String,
    pub disabled: bool,
    pub fetched_at: Option<f64>,
}

impl MenuAccount {
    pub fn name(&self) -> String {
        if self.alias.is_empty() {
            self.email.clone()
        } else {
            format!("{}  ({})", self.alias, self.email)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuSnapshot {
    pub accounts: Vec<MenuAccount>,
    pub active_email: Option<String>,
    pub active_usage: DisplayUsage,
    pub active_alias: Option<String>,
    /// Numbered apart from `accounts`; never disabled.
    pub codex: Vec<MenuAccount>,
}

impl Default for MenuSnapshot {
    fn default() -> Self {
        MenuSnapshot {
            accounts: Vec::new(),
            active_email: None,
            active_usage: DisplayUsage::Unavailable,
            active_alias: None,
            codex: Vec::new(),
        }
    }
}

pub fn codex_accounts(
    listing: &CodexListing,
    usage: &HashMap<u32, UsageEntry>,
) -> Vec<MenuAccount> {
    listing
        .accounts
        .iter()
        .map(|account| {
            let entry = usage.get(&account.number);
            MenuAccount {
                number: account.number.to_string(),
                email: account.email.clone(),
                is_active: account.active,
                display: entry.map_or(DisplayUsage::Unavailable, |e| match e.sentinel {
                    Some(s) => DisplayUsage::Note(sentinel_note(s).to_string()),
                    None => DisplayUsage::from_entry(e),
                }),
                last_good: entry.and_then(|e| e.last_good.clone()),
                alias: account.alias.clone().unwrap_or_default(),
                disabled: false,
                fetched_at: entry.and_then(|e| e.fetched_at),
            }
        })
        .collect()
}

impl MenuSnapshot {
    pub fn adapt(snap: &AccountsSnapshot) -> Self {
        let mut out = MenuSnapshot::default();
        for acc in &snap.accounts {
            let display = DisplayUsage::from_entry(&acc.usage);
            if acc.is_active {
                out.active_email = Some(acc.email.clone());
                out.active_usage = display.clone();
                out.active_alias = Some(acc.alias.clone());
            }
            out.accounts.push(MenuAccount {
                number: acc.number.clone(),
                email: acc.email.clone(),
                is_active: acc.is_active,
                display,
                last_good: acc.usage.last_good.clone(),
                alias: acc.alias.clone(),
                disabled: acc.disabled,
                fetched_at: acc.usage.fetched_at,
            });
        }
        out
    }
}

/// Python `isinstance(x, (int, float))`, bools included.
fn py_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        other => other.as_f64(),
    }
}

pub(super) fn pct_of(window: Option<&Value>) -> Option<f64> {
    py_number(window?.as_object()?.get("pct"))
}

fn window_pct(usage: Option<&Value>, key: &str) -> Option<f64> {
    pct_of(usage?.get(key))
}

fn pct(value: f64) -> String {
    format!("{value:.0}")
}

/// `inf` when `resets_at` is missing or unparseable.
pub(super) fn resets_at_ts(window: Option<&Value>) -> f64 {
    window
        .and_then(|w| w.as_object()?.get("resets_at")?.as_str())
        .and_then(parse_iso)
        .and_then(|t| t.timestamp())
        .unwrap_or(f64::INFINITY)
}

/// Reads `resets_at` because the stored countdown is frozen at fetch time.
fn live_countdown(window: Option<&Value>, now: f64) -> Option<String> {
    let ts = resets_at_ts(window);
    if ts.is_infinite() {
        return None;
    }
    let remaining = (ts - now).trunc() as i64;
    if remaining <= 0 {
        return None;
    }
    let (days, rem) = (remaining / 86400, remaining % 86400);
    let (hours, minutes) = (rem / 3600, rem % 3600 / 60);
    Some(if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    })
}

/// Weekly limits reset on a fixed 7-day cadence: a window whose reset has
/// passed is shown as reset (0%, next boundary) without a refetch.
pub(super) fn rolled_weekly_window(window: Option<&Value>, now: f64) -> Option<Value> {
    let window = window?;
    let Some(map) = window.as_object() else {
        return Some(window.clone());
    };
    let ts = resets_at_ts(Some(window));
    if ts.is_infinite() || ts > now {
        return Some(window.clone());
    }
    let missed = ((now - ts) / WEEKLY_PERIOD_S).floor() + 1.0;
    let mut rolled = map.clone();
    rolled.insert("pct".into(), 0.0.into());
    rolled.insert(
        "resets_at".into(),
        utc_isoformat(ts + missed * WEEKLY_PERIOD_S).into(),
    );
    rolled.shift_remove("countdown");
    rolled.shift_remove("clock");
    Some(Value::Object(rolled))
}

/// `datetime.fromtimestamp(ts, timezone.utc).isoformat()`.
fn utc_isoformat(ts: f64) -> String {
    let micros = (ts * 1e6).round() as i64;
    let dt = DateTime::<Utc>::from_timestamp_micros(micros).unwrap_or_default();
    let format = if micros % 1_000_000 == 0 {
        SecondsFormat::Secs
    } else {
        SecondsFormat::Micros
    };
    dt.to_rfc3339_opts(format, false)
}

fn ahead(window: Option<&Value>, fetched_at: Option<f64>) -> bool {
    compute_pace(window.and_then(Value::as_object), fetched_at).is_some_and(|p| p.ahead)
}

pub(super) fn scoped_windows(usage: &Value) -> impl Iterator<Item = &Value> {
    usage
        .get("scoped")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

pub(super) fn named_pct(window: &Map<String, Value>) -> Option<(&str, f64)> {
    let name = window.get("name").and_then(Value::as_str)?;
    let pct = py_number(window.get("pct"))?;
    (!name.is_empty()).then_some((name, pct))
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageKind {
    Session,
    Weekly,
    Scoped(String),
    Spend,
}

/// One usage window as the dropdown shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub kind: UsageKind,
    pub pct: f64,
    pub ahead: bool,
    pub countdown: Option<String>,
}

impl UsageRow {
    pub fn name(&self) -> &str {
        match &self.kind {
            UsageKind::Session => "Session",
            UsageKind::Weekly => "All",
            UsageKind::Scoped(name) => name,
            UsageKind::Spend => "Spend",
        }
    }

    fn short_name(&self) -> &str {
        match &self.kind {
            UsageKind::Session => "5h",
            UsageKind::Weekly => "7d",
            UsageKind::Scoped(name) => name,
            UsageKind::Spend => "$",
        }
    }

    pub fn maxed(&self) -> bool {
        self.pct >= 100.0
    }

    pub fn pct_text(&self) -> String {
        format!("{}%", pct(self.pct))
    }

    fn summary(&self) -> String {
        let mut seg = format!("{} {}", self.short_name(), self.pct_text());
        if matches!(self.kind, UsageKind::Scoped(_)) && self.maxed() {
            seg.push_str(" (!)");
        } else if self.ahead {
            seg.push_str(" (ahead)");
        }
        if let Some(countdown) = &self.countdown {
            seg.push_str(&format!(" ({countdown})"));
        }
        seg
    }
}

/// `Err` is the note shown instead of usage. `fetched_at` only drives the
/// weekly "ahead of pace" marker.
pub fn usage_rows(
    usage: &DisplayUsage,
    now: f64,
    fetched_at: Option<f64>,
) -> std::result::Result<Vec<UsageRow>, String> {
    let usage = match usage {
        DisplayUsage::Note(note) => return Err(note.clone()),
        DisplayUsage::Unavailable => return Err("usage unavailable".into()),
        DisplayUsage::Measured(usage) => usage,
    };
    let mut rows = Vec::new();
    let session = usage.get("five_hour");
    if let Some(p) = pct_of(session) {
        rows.push(UsageRow {
            kind: UsageKind::Session,
            pct: p,
            ahead: false,
            countdown: live_countdown(session, now),
        });
    }
    let weekly = rolled_weekly_window(usage.get("seven_day"), now);
    if let Some(p) = pct_of(weekly.as_ref()) {
        rows.push(UsageRow {
            kind: UsageKind::Weekly,
            pct: p,
            ahead: ahead(weekly.as_ref(), fetched_at),
            countdown: live_countdown(weekly.as_ref(), now),
        });
    }
    for window in scoped_windows(usage) {
        let rolled = rolled_weekly_window(Some(window), now);
        let Some((name, p)) = rolled
            .as_ref()
            .and_then(Value::as_object)
            .and_then(named_pct)
        else {
            continue;
        };
        rows.push(UsageRow {
            kind: UsageKind::Scoped(name.to_string()),
            pct: p,
            ahead: ahead(rolled.as_ref(), fetched_at),
            countdown: live_countdown(rolled.as_ref(), now),
        });
    }
    if let Some(p) = window_pct(Some(usage), "spend") {
        rows.push(UsageRow {
            kind: UsageKind::Spend,
            pct: p,
            ahead: false,
            countdown: None,
        });
    }
    if rows.is_empty() {
        Err("usage unavailable".into())
    } else {
        Ok(rows)
    }
}

pub fn usage_summary(usage: &DisplayUsage, now: f64, fetched_at: Option<f64>) -> String {
    match usage_rows(usage, now, fetched_at) {
        Ok(rows) => rows
            .iter()
            .map(UsageRow::summary)
            .collect::<Vec<_>>()
            .join(SEPARATOR),
        Err(note) => note,
    }
}

pub fn format_account_label(account: &MenuAccount, now: f64) -> String {
    let marker = if account.disabled { "  (disabled)" } else { "" };
    format!(
        "{}  {}{marker}  {}",
        account.number,
        account.name(),
        usage_summary(&account.display, now, account.fetched_at)
    )
}

pub(super) fn local_part(email: &str) -> String {
    const LIMIT: usize = 12;
    let local = email.split('@').next().unwrap_or_default();
    if local.chars().count() > LIMIT {
        local.chars().take(LIMIT - 1).chain(['*']).collect()
    } else {
        local.to_string()
    }
}

pub fn format_title(
    active_email: Option<&str>,
    usage: &DisplayUsage,
    settings: &MenuBarSettings,
    now: f64,
    alias: Option<&str>,
) -> String {
    let Some(email) = active_email else {
        return ICON.into();
    };
    let usage = usage.measured();
    let mut segments = Vec::new();
    if settings.show_account_name {
        segments.push(match alias.filter(|a| !a.is_empty()) {
            Some(alias) => alias.to_string(),
            None => local_part(email),
        });
    }
    if matches!(settings.title_pct.as_str(), "5h" | "both")
        && let Some(p) = window_pct(usage, "five_hour")
    {
        segments.push(format!("{}%", pct(p)));
    }
    if matches!(settings.title_pct.as_str(), "7d" | "both") {
        let rolled = rolled_weekly_window(usage.and_then(|u| u.get("seven_day")), now);
        if let Some(p) = pct_of(rolled.as_ref()) {
            segments.push(format!("{}%", pct(p)));
        }
    }
    if settings.title_scoped
        && let Some(usage) = usage
    {
        for window in scoped_windows(usage) {
            let rolled = rolled_weekly_window(Some(window), now);
            if let Some((name, p)) = rolled
                .as_ref()
                .and_then(Value::as_object)
                .and_then(named_pct)
            {
                segments.push(format!("{name} {}%", pct(p)));
            }
        }
    }
    if segments.is_empty() {
        ICON.into()
    } else {
        format!("{ICON} {}", segments.join(SEPARATOR))
    }
}

/// An account's 5h/7d line for `claude-swap.log`; `None` without a
/// numeric window.
pub fn format_usage_log(email: &str, usage: Option<&Value>) -> Option<String> {
    let mut parts = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        let Some(p) = window_pct(usage, key) else {
            continue;
        };
        let mut seg = format!("{label} {}%", pct(p));
        let clock = usage
            .and_then(|u| u.get(key)?.get("clock")?.as_str())
            .filter(|c| !c.is_empty());
        if let Some(clock) = clock {
            seg.push_str(&format!(" (resets {clock})"));
        }
        parts.push(seg);
    }
    (!parts.is_empty()).then(|| format!("usage {email}: {}", parts.join(SEPARATOR)))
}

/// De-dupes usage logging on the percentages only: reset clocks change
/// every refresh.
pub fn usage_log_key(usage: Option<&Value>) -> (Option<f64>, Option<f64>) {
    (
        window_pct(usage, "five_hour"),
        window_pct(usage, "seven_day"),
    )
}

/// `from → to   YYYY-MM-DD HH:MM` for the log's `Switched from account X
/// to Y` lines, most recent first.
pub fn parse_switch_history(log_text: &str, limit: usize) -> Vec<String> {
    let entries: Vec<String> = log_text
        .lines()
        .filter_map(|line| {
            let (from, to) = switched_accounts(line)?;
            let stamp: String = line
                .split(" - ")
                .next()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(16)
                .collect();
            Some(format!("{from} → {to}   {stamp}"))
        })
        .collect();
    let start = entries.len().saturating_sub(limit);
    entries[start..].iter().rev().cloned().collect()
}

/// The first `Switched from account (\d+) to (\d+)` match in a line.
fn switched_accounts(line: &str) -> Option<(&str, &str)> {
    const PREFIX: &str = "Switched from account ";
    line.match_indices(PREFIX).find_map(|(at, _)| {
        let rest = &line[at + PREFIX.len()..];
        let (from, rest) = split_digits(rest)?;
        let (to, _) = split_digits(rest.strip_prefix(" to ")?)?;
        Some((from, to))
    })
}

fn split_digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

#[cfg(test)]
mod tests;
