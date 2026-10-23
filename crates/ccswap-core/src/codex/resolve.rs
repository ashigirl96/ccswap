//! A Codex account number from what the user typed, by the Claude side's rules
//! (`store::resolve_account_identifier`): a number, then an alias, then an email.

use crate::error::{Error, ErrorKind, Result};
use crate::store::is_slot_number;

use super::ledger::Ledger;

/// An email shared by several accounts (workspaces) is an error, never a guess.
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
    let wanted = identifier.trim().to_lowercase();
    if let Some((number, _)) = ledger.accounts.iter().find(|(_, a)| {
        a.alias
            .as_deref()
            .is_some_and(|alias| alias.to_lowercase() == wanted)
    }) {
        return Ok(*number);
    }
    let matches: Vec<(u32, &str)> = ledger
        .ordered()
        .into_iter()
        .filter(|(_, a)| !a.email.is_empty() && a.email.to_lowercase() == wanted)
        .map(|(n, a)| (n, a.plan.as_str()))
        .collect();
    match matches.as_slice() {
        [] => Err(Error::new(
            ErrorKind::AccountNotFoundError,
            format!("No Codex account found with identifier: {identifier}"),
        )),
        [(number, _)] => Ok(*number),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::LedgerAccount;

    fn ledger(rows: &[(u32, &str, &str, Option<&str>)]) -> Ledger {
        let mut ledger = Ledger::default();
        for (n, email, plan, alias) in rows {
            ledger.insert(
                *n,
                LedgerAccount {
                    email: (*email).into(),
                    account_id: format!("acct-{n}"),
                    plan: (*plan).into(),
                    alias: alias.map(str::to_string),
                    added: String::new(),
                    extra: Default::default(),
                },
            );
        }
        ledger
    }

    #[test]
    fn numbers_aliases_and_emails_resolve_and_unknowns_are_not_found() {
        let l = ledger(&[
            (1, "a@example.com", "plus", None),
            (2, "b@example.com", "pro", Some("work")),
        ]);
        assert_eq!(resolve_account(&l, "2").unwrap(), 2);
        assert_eq!(resolve_account(&l, "WORK").unwrap(), 2);
        assert_eq!(resolve_account(&l, "A@example.com").unwrap(), 1);
        let missing = resolve_account(&l, "3").unwrap_err();
        assert_eq!(missing.kind, ErrorKind::AccountNotFoundError);
        assert_eq!(missing.to_string(), "Codex account 3 does not exist");
        let unknown = resolve_account(&l, "c@example.com").unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "No Codex account found with identifier: c@example.com"
        );
    }

    #[test]
    fn an_email_in_two_workspaces_is_ambiguous() {
        let l = ledger(&[
            (1, "a@example.com", "plus", None),
            (2, "a@example.com", "team", None),
        ]);
        let err = resolve_account(&l, "a@example.com").unwrap_err();
        assert!(
            err.to_string()
                .contains("matches Codex accounts: 1 [plus], 2 [team]"),
            "{err}"
        );
    }
}
