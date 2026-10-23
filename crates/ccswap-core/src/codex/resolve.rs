//! `<num|alias|email>` to a Codex account number, by the Claude side's rules
//! (`store::resolve_account_identifier`): a number first, then an alias, then an email,
//! and an email in several accounts is an error rather than a guess.

use crate::error::{Error, ErrorKind, Result};
use crate::store::{is_slot_number, is_valid_email};

use super::ledger::Ledger;

/// The number of an account present in `ledger`.
pub fn resolve_account(ledger: &Ledger, identifier: &str) -> Result<u32> {
    if is_slot_number(identifier) {
        return identifier
            .parse::<u32>()
            .ok()
            .filter(|n| ledger.accounts.contains_key(n))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::AccountNotFoundError,
                    format!("Codex account {identifier} does not exist"),
                )
            });
    }
    if let Some((n, _)) = ledger
        .ordered()
        .into_iter()
        .find(|(_, a)| a.alias.as_deref() == Some(identifier))
    {
        return Ok(n);
    }
    if !is_valid_email(identifier) {
        return Err(Error::new(
            ErrorKind::ValidationError,
            format!("Invalid account identifier: {identifier}"),
        ));
    }
    let wanted = identifier.trim().to_lowercase();
    let matches: Vec<(u32, &str)> = ledger
        .ordered()
        .into_iter()
        .filter(|(_, a)| a.email == wanted)
        .map(|(n, a)| (n, a.plan.as_str()))
        .collect();
    match matches.as_slice() {
        [] => Err(Error::new(
            ErrorKind::AccountNotFoundError,
            format!("No Codex account found with identifier: {identifier}"),
        )),
        [(n, _)] => Ok(*n),
        many => {
            let details: Vec<String> = many
                .iter()
                .map(|(n, plan)| {
                    let plan = if plan.is_empty() {
                        "unknown plan"
                    } else {
                        plan
                    };
                    format!("{n} [{plan}]")
                })
                .collect();
            Err(Error::config(format!(
                "Email '{identifier}' is ambiguous — matches Codex accounts: {}. Use the account number instead (e.g., ccswap codex switch {}).",
                details.join(", "),
                many[0].0
            )))
        }
    }
}
