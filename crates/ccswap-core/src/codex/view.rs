//! Terminal lines and the `--json` envelope for Codex accounts, shaped like the Claude side's.

use serde_json::{Map, Value, json};

use crate::presentation::{SCHEMA_VERSION, Style};

use super::store::{AddOutcome, CodexListing};

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
