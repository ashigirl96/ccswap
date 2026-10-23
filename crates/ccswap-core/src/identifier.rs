//! The account a user names by number, alias or email: the rules both the Claude
//! store and the Codex store follow.

use crate::accounts::Console;
use crate::error::{Error, ErrorKind, Result};
use crate::presentation::Style;
use crate::store::{is_slot_number, is_valid_email};

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
