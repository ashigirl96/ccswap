//! Terminal lines and the `--json` envelope for Codex accounts, shaped like the Claude side's.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::poll_policy::SERVE_TTL_S;
use crate::presentation::{SCHEMA_VERSION, Style};
use crate::usage_store::UsageEntry;
use crate::usage_view::{format_age, format_usage_lines, insert_usage_fields, usage_entry_lines};

use super::store::{AddOutcome, CodexListing};
use super::usage::WINDOWS;

/// Each account's row, then its usage lines; `usage` lacks accounts that were not looked up.
pub fn list_lines(
    listing: &CodexListing,
    usage: &HashMap<u32, UsageEntry>,
    style: &Style,
) -> Vec<String> {
    if listing.accounts.is_empty() {
        return vec![style.dimmed(
            "No Codex accounts are managed yet. Run 'ccswap codex add' to register the current login.",
        )];
    }
    let mut lines = vec![style.bolded("Codex accounts:")];
    for (i, account) in listing.accounts.iter().enumerate() {
        let email = display_email(&account.email);
        let label = match &account.alias {
            Some(alias) => format!("{} ({email})", style.accent(alias)),
            None => email,
        };
        let plan = display_plan(&account.plan);
        let active = if account.active {
            format!(" {}", style.bold_accent("(active)"))
        } else {
            String::new()
        };
        lines.push(format!(
            "  {}: {label} {}{active}",
            account.number,
            style.muted(&format!("[{plan}]"))
        ));
        if let Some(entry) = usage.get(&account.number) {
            lines.extend(
                usage_lines(entry, style)
                    .into_iter()
                    .map(|l| format!("     {l}")),
            );
            if i + 1 < listing.accounts.len() {
                lines.push(String::new());
            }
        }
    }
    lines
}

/// Like the Claude side's, but every window is shown and a missing one reads `unknown`.
fn usage_lines(entry: &UsageEntry, style: &Style) -> Vec<String> {
    let Some(last_good) = entry
        .last_good
        .as_ref()
        .filter(|_| entry.sentinel.is_none())
    else {
        return usage_entry_lines(entry, style);
    };
    let mut rows: Vec<String> = WINDOWS
        .iter()
        .map(|(label, key, _)| match last_good.get(key) {
            Some(window) => format_usage_lines(&json!({ *key: window }), entry.fetched_at)
                .pop()
                .unwrap_or_else(|| format!("{label}: unknown")),
            None => format!("{label}: unknown"),
        })
        .collect();
    if let (Some(last), Some(age), Some(fetched_at)) =
        (rows.last_mut(), entry.age_s, entry.fetched_at)
        && age > SERVE_TTL_S
    {
        last.push_str(&format!(" · {}", format_age(fetched_at)));
    }
    let n = rows.len();
    rows.iter()
        .enumerate()
        .map(|(j, row)| {
            let connector = if j + 1 == n { "└" } else { "├" };
            format!("{} {}", style.dimmed(connector), style.muted(row))
        })
        .collect()
}

/// `null` stands for a window the response did not carry in a known shape.
fn mark_unknown_windows(windows: &mut Map<String, Value>) {
    for key in ["fiveHour", "sevenDay"] {
        windows.entry(key).or_insert(Value::Null);
    }
}

pub fn list_json(listing: &CodexListing, usage: &HashMap<u32, UsageEntry>) -> Value {
    let accounts: Vec<Value> = listing
        .accounts
        .iter()
        .map(|a| {
            let mut row = Map::new();
            row.insert("number".into(), a.number.into());
            row.insert("email".into(), a.email.clone().into());
            row.insert("accountId".into(), a.account_id.clone().into());
            row.insert("plan".into(), a.plan.clone().into());
            row.insert("active".into(), a.active.into());
            if let Some(entry) = usage.get(&a.number) {
                insert_usage_fields(&mut row, entry, "", false);
                for key in ["usage", "lastGoodUsage"] {
                    if let Some(Value::Object(windows)) = row.get_mut(key) {
                        mark_unknown_windows(windows);
                    }
                }
            }
            if let Some(alias) = &a.alias {
                row.insert("alias".into(), alias.clone().into());
            }
            Value::Object(row)
        })
        .collect();
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "activeAccountNumber": listing.active_number(),
        "accounts": accounts,
    })
}

pub fn add_lines(outcome: &AddOutcome, style: &Style) -> Vec<String> {
    match outcome {
        AddOutcome::Added { number, identity } => vec![format!(
            "{} {number}: {} {}",
            style.accent("Added Codex account"),
            display_email(&identity.email),
            style.muted(&format!("[{}]", display_plan(&identity.plan)))
        )],
        AddOutcome::AlreadyManaged { number, identity } => vec![style.yellowed(&format!(
            "Codex account {} is already managed as {number}. Nothing was added.",
            display_email(&identity.email)
        ))],
    }
}

fn display_plan(plan: &str) -> &str {
    if plan.is_empty() {
        "unknown plan"
    } else {
        plan
    }
}

fn display_email(email: &str) -> String {
    if email.is_empty() {
        "(no email)".into()
    } else {
        email.into()
    }
}
