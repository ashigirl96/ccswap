//! Terminal lines and the `--json` envelope for Codex accounts, shaped like the Claude side's.

use serde_json::{Map, Value, json};

use crate::presentation::{SCHEMA_VERSION, Style};

use super::login::LoginOutcome;
use super::store::{AddOutcome, CodexListing};
use super::switch::{SwitchOutcome, SwitchedAccount};

pub fn switch_lines(outcome: &SwitchOutcome, style: &Style) -> Vec<String> {
    if !outcome.switched {
        return vec![format!(
            "{} {}",
            style.accent("Already on"),
            switched_label(&outcome.to)
        )];
    }
    let mut lines = Vec::new();
    for path in &outcome.unclaimed {
        lines.push(style.yellowed(&format!(
            "The live Codex login is not a managed account; saved it to {}",
            path.display()
        )));
    }
    lines.push(format!(
        "{} {}",
        style.accent("Switched to"),
        switched_label(&outcome.to)
    ));
    lines.push(style.dimmed(
        "Takes effect in codex sessions started from now on; running ones keep the previous account.",
    ));
    if let Some(dir) = &outcome.daemon_dir {
        lines.push(style.dimmed(&format!(
            "The Codex app-server daemon ({}) keeps the previous account until restarted: codex app-server daemon restart",
            dir.display()
        )));
    }
    lines
}

/// The Claude switch envelope (`SwitchReport::to_json`) with Codex account numbers.
pub fn switch_json(outcome: &SwitchOutcome) -> Value {
    let reference = |a: &SwitchedAccount| json!({ "number": a.number, "email": a.email });
    let message = if outcome.switched {
        format!("Switched to {}", switched_label(&outcome.to))
    } else {
        format!("Already on {}", switched_label(&outcome.to))
    };
    let warnings: Vec<String> = outcome
        .unclaimed
        .iter()
        .map(|p| {
            format!(
                "The live Codex login is not a managed account; saved it to {}",
                p.display()
            )
        })
        .collect();
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "switched": outcome.switched,
        "from": outcome.from.as_ref().map_or(Value::Null, reference),
        "to": reference(&outcome.to),
        "strategy": outcome.strategy,
        "reason": outcome.reason,
        "message": message,
        "warnings": warnings,
    })
}

fn switched_label(account: &SwitchedAccount) -> String {
    let number = account.number.map_or("None".into(), |n| n.to_string());
    format!("Codex account {number} ({})", display_email(&account.email))
}

pub fn list_lines(listing: &CodexListing, style: &Style) -> Vec<String> {
    if listing.accounts.is_empty() {
        return vec![style.dimmed(
            "No Codex accounts are managed yet. Run 'ccswap codex add' to register the current login.",
        )];
    }
    let mut lines = vec![style.bolded("Codex accounts:")];
    for account in &listing.accounts {
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
    }
    lines
}

pub fn list_json(listing: &CodexListing) -> Value {
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

pub fn login_lines(outcome: &LoginOutcome, style: &Style) -> Vec<String> {
    match outcome {
        LoginOutcome::Added { number, identity } => add_lines(
            &AddOutcome::Added {
                number: *number,
                identity: identity.clone(),
            },
            style,
        ),
        LoginOutcome::Updated { number, identity } => vec![style.yellowed(&format!(
            "Codex account {} is already managed as {number}. Updated its stored login.",
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
