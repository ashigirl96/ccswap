//! The account a user names by number, alias or email: the rules both the Claude
//! store and the Codex store follow.

use crate::console::Console;
use crate::error::{Error, ErrorKind, Result};
use crate::presentation::Style;

/// Python's `str.isdigit()` for the identifiers users type.
pub fn is_slot_number(identifier: &str) -> bool {
    !identifier.is_empty() && identifier.chars().all(|c| c.is_ascii_digit())
}

/// `^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$` under `re.match`
/// (whose `$` also matches before one trailing newline).
pub fn is_valid_email(email: &str) -> bool {
    let email = email.strip_suffix('\n').unwrap_or(email);
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
    let domain_chars_ok = domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".-".contains(c));
    let Some((host, tld)) = domain.rsplit_once('.') else {
        return false;
    };
    local_ok
        && domain_chars_ok
        && !host.is_empty()
        && tld.len() >= 2
        && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// Lowercase and validate a proposed alias (`[a-z0-9_.-]+`, not all digits,
/// no leading `-`). Messages echo the raw input.
pub fn normalize_alias(raw: &str) -> Result<String> {
    let invalid = |m: String| Err(Error::new(ErrorKind::ValidationError, m));
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return invalid("alias cannot be empty".into());
    }
    if normalized.chars().all(|c| c.is_ascii_digit()) {
        return invalid(format!(
            "alias '{raw}' cannot be purely numeric (reserved for slot numbers)"
        ));
    }
    if normalized.starts_with('-') {
        return invalid(format!(
            "alias '{raw}' cannot start with '-' (would be read as a command flag)"
        ));
    }
    if !normalized
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
    {
        return invalid(format!(
            "alias '{raw}' may only contain letters, digits, '-', '_', and '.'"
        ));
    }
    Ok(normalized)
}

/// One managed account as identifier resolution sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub number: String,
    /// `None` when the record has no email.
    pub email: Option<String>,
    /// `""` when the account has none.
    pub alias: String,
    /// What tells accounts with the same email apart, e.g. `[Acme]`.
    pub tag: String,
}

pub enum Found<'a> {
    /// A number, returned verbatim and unchecked ("01", "99").
    Number(String),
    One(&'a Candidate),
    /// An email in several accounts.
    Ambiguous(Vec<&'a Candidate>),
    Missing,
}

/// A number, then an alias (case-insensitive), then an email (exact).
pub fn find<'a>(candidates: &'a [Candidate], identifier: &str) -> Found<'a> {
    if is_slot_number(identifier) {
        return Found::Number(identifier.to_string());
    }
    if let Some(candidate) = by_alias(candidates, identifier) {
        return Found::One(candidate);
    }
    let mut matches = with_email(candidates, identifier);
    match matches.len() {
        0 => Found::Missing,
        1 => Found::One(matches.remove(0)),
        _ => Found::Ambiguous(matches),
    }
}

fn by_alias<'a>(candidates: &'a [Candidate], identifier: &str) -> Option<&'a Candidate> {
    if identifier.is_empty() {
        return None;
    }
    let wanted = identifier.to_lowercase();
    candidates.iter().find(|c| c.alias.to_lowercase() == wanted)
}

fn with_email<'a>(candidates: &'a [Candidate], email: &str) -> Vec<&'a Candidate> {
    candidates
        .iter()
        .filter(|c| c.email.as_deref() == Some(email))
        .collect()
}

/// Asks in a terminal which of several accounts with one email is meant.
pub struct Picker<'a> {
    pub console: &'a mut dyn Console,
    pub style: &'a Style,
    /// Completes "Enter account number to …: ".
    pub action: &'a str,
}

/// Checks what the user typed before it is resolved: a word that is neither an
/// alias nor a valid email is refused, and with a `picker` an email in several
/// accounts is narrowed to the chosen number. `None` means the user cancelled.
pub fn screen(
    candidates: &[Candidate],
    identifier: &str,
    picker: Option<Picker<'_>>,
) -> Result<Option<String>> {
    if is_slot_number(identifier) {
        return Ok(Some(identifier.to_string()));
    }
    let is_alias = by_alias(candidates, identifier).is_some();
    if !is_alias && !is_valid_email(identifier) {
        return Err(Error::new(
            ErrorKind::ValidationError,
            format!("Invalid account identifier: {identifier}"),
        ));
    }
    let Some(picker) = picker.filter(|_| !is_alias) else {
        return Ok(Some(identifier.to_string()));
    };
    let matches = with_email(candidates, identifier);
    if matches.len() < 2 {
        return Ok(Some(identifier.to_string()));
    }
    let Picker {
        console,
        style,
        action,
    } = picker;
    console.line(&format!("Multiple accounts found for '{identifier}':"));
    for candidate in &matches {
        console.line(&format!(
            "  {}: {identifier} {}",
            candidate.number,
            style.muted(&format!("[{}]", candidate.tag))
        ));
    }
    let choice = console
        .prompt(&format!("Enter account number to {action}: "))
        .unwrap_or_default();
    let choice = choice.trim();
    if is_slot_number(choice) && matches.iter().any(|c| c.number == choice) {
        Ok(Some(choice.to_string()))
    } else {
        console.line(&style.dimmed("Cancelled"));
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_check_matches_the_upstream_pattern() {
        for ok in [
            "user@example.com",
            "a.b+c%d_e-f@sub.example.co",
            "x@a.b.cd",
            "x@a..cd",
            "setup-token-3@token.local",
            "user@example.com\n",
        ] {
            assert!(is_valid_email(ok), "{ok:?}");
        }
        for bad in [
            "not-an-email",
            "../../evil",
            "a@b",
            "@example.com",
            "user@.com",
            "user@example.c",
            "user@example.c0m",
            "us er@example.com",
            "a@b@example.com",
            "user@example.com\n\n",
            "ü@example.com",
        ] {
            assert!(!is_valid_email(bad), "{bad:?}");
        }
    }
}
