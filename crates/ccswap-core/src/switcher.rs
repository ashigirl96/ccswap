//! The switch: pick a target, then one transaction under the store lock and
//! Claude Code's locks that backs up the outgoing login, activates the
//! target and rolls every recorded step back if a later one fails.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::accounts::{Accounts, AddOptions, Console};
use crate::anthropic_client::{self, AnthropicClient, Profile};
use crate::env::Environment;
use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{self, STORE_LOCK_TIMEOUT, StoreLock};
use crate::log;
use crate::paths::Paths;
use crate::presentation::{SCHEMA_VERSION, Style};
use crate::pyjson;
use crate::ranking::{self, Strategy};
use crate::secret_store::{ActiveBackend, SecretStore};
use crate::store::{self, JsonObject};
use crate::unclaimed::UnclaimedStash;

mod strategy;

use strategy::Hook;
pub use strategy::{ModelSource, SwitchStrategy};

/// Credential siblings of `claudeAiOauth` owned by the machine rather than
/// an account: on activation the live copies win, absence included.
pub const SHARED_CREDENTIAL_KEYS: [&str; 5] = [
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

/// Prints the account list after a human-mode switch.
pub type Listing<'a> = dyn Fn(&mut dyn Console) -> Result<()> + 'a;

pub struct Switcher<'a> {
    pub paths: &'a Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
    pub style: &'a Style,
    /// Prints the account list after a human-mode switch; an error becomes
    /// a one-line notice instead of failing the switch.
    pub listing: Option<&'a Listing<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRef {
    pub number: Option<i64>,
    pub email: String,
}

impl AccountRef {
    fn slot(num: &str, email: &str) -> Self {
        AccountRef {
            number: num.parse().ok(),
            email: email.to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({"number": self.number, "email": self.email})
    }
}

/// The `--json` result of a switch, applied or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchReport {
    pub switched: bool,
    pub from: Option<AccountRef>,
    pub to: Option<AccountRef>,
    pub strategy: &'static str,
    pub reason: &'static str,
    pub message: String,
    pub warnings: Vec<String>,
}

impl SwitchReport {
    fn noop(
        strategy: &'static str,
        reason: &'static str,
        to: Option<AccountRef>,
        message: String,
        warnings: Vec<String>,
    ) -> Self {
        SwitchReport {
            switched: false,
            from: to.clone(),
            to,
            strategy,
            reason,
            message,
            warnings,
        }
    }

    fn from_performed(
        performed: Performed,
        strategy: &'static str,
        mut warnings: Vec<String>,
    ) -> Self {
        let to = performed.to;
        let switched = performed.from.as_ref() != Some(&to);
        let (reason, verb) = if switched {
            ("switched", "Switched to")
        } else {
            ("already-active", "Already on")
        };
        warnings.extend(performed.warnings);
        SwitchReport {
            switched,
            message: format!(
                "{verb} Account-{} ({})",
                to.number.map_or("None".into(), |n| n.to_string()),
                to.email
            ),
            from: performed.from,
            to: Some(to),
            strategy,
            reason,
            warnings,
        }
    }

    pub fn to_json(&self) -> Value {
        let reference =
            |r: &Option<AccountRef>| r.as_ref().map_or(Value::Null, AccountRef::to_json);
        json!({
            "schemaVersion": SCHEMA_VERSION,
            "switched": self.switched,
            "from": reference(&self.from),
            "to": reference(&self.to),
            "strategy": self.strategy,
            "reason": self.reason,
            "message": self.message,
            "warnings": self.warnings,
        })
    }
}

/// What the transaction did, captured under the locks.
struct Performed {
    from: Option<AccountRef>,
    to: AccountRef,
    warnings: Vec<String>,
}

/// The pre-lock answer to "whose are the live bytes?". `resolved` only
/// describes `live`; once the live store has moved it says nothing.
#[derive(Debug, Clone, Default)]
struct Provenance {
    live: Option<String>,
    resolved: Option<Profile>,
}

/// Whose the live credential is, judged against the outgoing slot's backup.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outgoing {
    /// Byte-identical to the backup: only the config needs refreshing.
    OwnBytes,
    /// Same refresh-token lineage: back up the newer generation.
    OwnFamily,
    /// A new lineage the profile endpoint attributes to the outgoing slot.
    OwnRotated,
    /// Claude Code emptied the tokens after a rejected refresh; writing
    /// them would destroy the slot's only refresh token.
    Wiped,
    /// Lineage differs and the owner is unknown: back up (fail open).
    Unresolved,
    /// A complete identity that is no managed slot's: stash, never back up.
    Alien,
    /// Positively another slot's, a lineage that slot does not hold: stash.
    Foreign(String),
    /// Another slot's, and that slot already holds this lineage.
    ForeignSynced(String),
}

impl Outgoing {
    fn stash_reason(&self) -> Option<&'static str> {
        match self {
            Outgoing::Alien => Some("alien"),
            Outgoing::Foreign(_) => Some("foreign"),
            _ => None,
        }
    }
}

fn same_lineage(a: &str, b: &str) -> bool {
    a == b
        || anthropic_client::credential_fingerprint(a)
            == anthropic_client::credential_fingerprint(b)
}

fn profile_json(profile: &Profile) -> Value {
    json!({
        "uuid": profile.uuid,
        "email": profile.email,
        "organizationUuid": profile.organization_uuid,
    })
}

fn record_text<'d>(data: &'d JsonObject, num: &str, key: &str) -> &'d str {
    store::accounts(data)
        .and_then(|a| a.get(num))
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    CredentialsWritten,
    ConfigWritten,
    SequenceUpdated,
}

impl Step {
    fn name(&self) -> &'static str {
        match self {
            Step::CredentialsWritten => "credentials_written",
            Step::ConfigWritten => "config_written",
            Step::SequenceUpdated => "sequence_updated",
        }
    }
}

/// The live state before the switch and the steps applied since, undone
/// newest first.
struct SwitchTransaction {
    original_credentials: String,
    original_config: String,
    original_account: String,
    config_path: PathBuf,
    completed: Vec<Step>,
}

impl SwitchTransaction {
    fn record(&mut self, step: Step) {
        self.completed.push(step);
    }

    /// `true` when every step was undone.
    fn rollback(&self, switcher: &Switcher) -> bool {
        let mut ok = true;
        for step in self.completed.iter().rev() {
            let undone = match step {
                Step::CredentialsWritten => switcher
                    .secrets
                    .write_active(&self.original_credentials)
                    .map(drop),
                Step::ConfigWritten => std::fs::write(&self.config_path, &self.original_config)
                    .and_then(|()| fsutil::chmod(&self.config_path, 0o600))
                    .map_err(|e| Error::config(e.to_string())),
                Step::SequenceUpdated => self.restore_active_number(switcher.paths),
            };
            match undone {
                Ok(()) => log::info(format!("Rolled back step: {}", step.name())),
                Err(e) => {
                    log::error(format!("Failed to rollback step {}: {e}", step.name()));
                    ok = false;
                }
            }
        }
        ok
    }

    fn restore_active_number(&self, paths: &Paths) -> Result<()> {
        let Some(mut data) = store::read_sequence(paths)?.filter(|d| !d.is_empty()) else {
            return Ok(());
        };
        let number: i64 = self
            .original_account
            .parse()
            .map_err(|_| Error::config("invalid account number"))?;
        data.insert("activeAccountNumber".into(), number.into());
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(paths, &data)
    }
}

/// The store `.lock`, then Claude Code's credential pair, then its config
/// lock; released in reverse.
struct TransactionLocks {
    _config: locks::DirLock,
    _credentials: locks::ClaudeCredentialsLock,
    _store: StoreLock,
}

impl Switcher<'_> {
    fn accounts(&self) -> Accounts<'_> {
        Accounts {
            paths: self.paths,
            env: self.env,
            secrets: self.secrets,
            client: self.client,
            style: self.style,
        }
    }

    /// Bare `switch`: the next enabled, switchable slot in `sequence` order
    /// after the recorded active one, or the pick of a usage-aware strategy.
    pub fn switch_next(
        &self,
        strategy: Option<&SwitchStrategy>,
        json: bool,
        console: &mut dyn Console,
    ) -> Result<Option<SwitchReport>> {
        let label = strategy.map_or("rotation", |s| s.strategy.label());
        let models: &[String] = strategy.map_or(&[], |s| &s.models);
        let style = self.style;
        let mut warnings = Vec::new();
        self.announce_models(strategy, json, console);
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        let identity = store::current_identity(self.paths);
        store::read_sequence_migrated(self.paths)?;

        let Some(identity) = identity else {
            let target = self.fresh_machine_target(json, &mut warnings, console)?;
            let performed = self.perform(&target, !json, false, None, console)?;
            return Ok(json.then(|| SwitchReport::from_performed(performed, "rotation", warnings)));
        };
        let email = identity.email.clone();
        let data = self.roster()?;
        let Some(live_slot) = store::find_account_slot(&data, &email, &identity.organization_uuid)
        else {
            if json {
                let unmanaged = Some(AccountRef {
                    number: None,
                    email,
                });
                return Ok(Some(SwitchReport::noop(
                    label,
                    "unmanaged-account",
                    unmanaged,
                    "Active account is not managed; run cswap --add-account".into(),
                    warnings,
                )));
            }
            console.line(&format!(
                "{} Active account '{email}' was not managed.",
                style.accent("Notice:")
            ));
            self.accounts()
                .add_account(&AddOptions::default(), console)?;
            let active = self
                .roster()?
                .get("activeAccountNumber")
                .map_or("None".into(), python_str);
            console.line(&format!(
                "It has been automatically added as Account-{active}."
            ));
            console.line(
                &style.dimmed("Please run the switch command again to switch to the next account."),
            );
            return Ok(None);
        };

        let sequence = data
            .get("sequence")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if sequence.len() < 2 {
            let message = "Only one account is managed. Add more accounts to switch between.";
            if json {
                return Ok(Some(SwitchReport::noop(
                    label,
                    "only-one-account",
                    Some(AccountRef::slot(&live_slot, &email)),
                    message.into(),
                    warnings,
                )));
            }
            console.line(&style.dimmed(message));
            return Ok(None);
        }

        let current_ref = AccountRef::slot(&live_slot, &email);
        let next_available = strategy.is_some_and(|s| s.strategy == Strategy::NextAvailable);
        if strategy.is_some_and(|s| s.strategy == Strategy::Best)
            && let Hook::Done(report) = self.switch_best(
                &data,
                &live_slot,
                &current_ref,
                models,
                json,
                &mut warnings,
                console,
            )?
        {
            return Ok(report);
        }
        let active = data.get("activeAccountNumber");
        let position =
            |n: Option<i64>| n.and_then(|n| sequence.iter().position(|v| v.as_i64() == Some(n)));
        let anchor = if next_available {
            live_slot.parse().ok()
        } else {
            active.and_then(slot_number)
        };
        let start = position(anchor)
            .or_else(|| position(active.and_then(slot_number)))
            .unwrap_or(0);
        let usage = next_available.then(|| self.decision_usage(&data));
        if let Some(usage) = &usage {
            self.warn_inert_models(usage, models, json, &mut warnings, console);
        }
        let mut skipped_exhausted = false;

        let mut next = None;
        for offset in 1..sequence.len() {
            let candidate = python_str(&sequence[(start + offset) % sequence.len()]);
            if is_disabled(&data, &candidate) {
                if json {
                    warnings.push(format!("Skipped Account-{candidate} (disabled)"));
                } else {
                    console.line(&format!(
                        "{} Account-{candidate} (disabled)",
                        style.accent("Skipping")
                    ));
                }
                continue;
            }
            if !self.is_switchable(&data, &candidate) {
                if json {
                    warnings.push(format!(
                        "Skipped Account-{candidate} (no stored credentials/config)"
                    ));
                } else {
                    console.line(&format!(
                        "{} Account-{candidate} (no stored credentials/config, re-add with cswap --add-account --slot {candidate})",
                        style.accent("Skipping")
                    ));
                }
                continue;
            }
            if let Some(at) = usage.as_ref().and_then(|u| {
                ranking::at_limit_label(u.get(&candidate).and_then(Option::as_ref), models)
            }) {
                skipped_exhausted = true;
                let message = format!("Account-{candidate} (at {at} limit)");
                if json {
                    warnings.push(format!("Skipped {message}"));
                } else {
                    console.line(&format!("{} {message}", style.accent("Skipping")));
                }
                continue;
            }
            next = Some(candidate);
            break;
        }

        if next.is_none() && skipped_exhausted {
            let message = format!(
                "All other accounts are at their {} — staying on Account-{live_slot}.",
                strategy::limits_label(models)
            );
            if json {
                return Ok(Some(SwitchReport::noop(
                    label,
                    "candidates-exhausted",
                    Some(current_ref),
                    message,
                    warnings,
                )));
            }
            console.line(&style.yellowed(&message));
            return Ok(None);
        }

        let Some(next) = next else {
            if json {
                return Ok(Some(SwitchReport::noop(
                    label,
                    "no-valid-target",
                    Some(current_ref),
                    "No other accounts have valid stored credentials/config.".into(),
                    warnings,
                )));
            }
            console.line(&style.dimmed(
                "No other accounts have valid stored credentials/config.\nRe-add a skipped slot with: cswap --add-account --slot <number>",
            ));
            return Ok(None);
        };

        let mut provenance = None;
        if next == live_slot {
            provenance = self.self_switch_reconciliation(&next, &email);
        }
        if next == live_slot && provenance.is_none() {
            let message = format!("Already on Account-{next} ({email})");
            if json {
                return Ok(Some(SwitchReport::noop(
                    label,
                    "already-active",
                    Some(current_ref),
                    message,
                    warnings,
                )));
            }
            console.line(&format!(
                "{} Account-{next} ({email})",
                style.accent("Already on")
            ));
            return Ok(None);
        }

        let performed = self.perform(&next, !json, false, provenance, console)?;
        Ok(json.then(|| SwitchReport::from_performed(performed, label, warnings)))
    }

    /// With no live login there is nothing to back up: the recorded active
    /// slot (else the first in `sequence`) is activated, or the first
    /// usable slot after it when it is disabled or has no backups.
    fn fresh_machine_target(
        &self,
        json: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<String> {
        let data = self.roster()?;
        let sequence: Vec<String> = data
            .get("sequence")
            .and_then(Value::as_array)
            .map(|s| s.iter().map(python_str).collect())
            .unwrap_or_default();
        let preferred = data
            .get("activeAccountNumber")
            .filter(|v| truthy(v))
            .map(python_str)
            .or_else(|| {
                data.get("sequence")
                    .and_then(Value::as_array)
                    .and_then(|s| s.first())
                    .filter(|v| truthy(v))
                    .map(python_str)
            })
            .ok_or_else(|| Error::config("No accounts are managed yet"))?;

        let disabled = is_disabled(&data, &preferred);
        if !disabled && self.is_switchable(&data, &preferred) {
            return Ok(preferred);
        }
        let (reason, console_reason) = if disabled {
            ("(disabled)".to_string(), "(disabled)".to_string())
        } else {
            (
                "(no stored credentials/config)".to_string(),
                format!(
                    "(no stored credentials/config, re-add with cswap --add-account --slot {preferred})"
                ),
            )
        };
        if json {
            warnings.push(format!("Skipped Account-{preferred} {reason}"));
        } else {
            console.line(&format!(
                "{} Account-{preferred} {console_reason}",
                self.style.accent("Skipping")
            ));
        }
        if let Some(fallback) = sequence.iter().find(|num| {
            **num != preferred && !is_disabled(&data, num) && self.is_switchable(&data, num)
        }) {
            return Ok(fallback.clone());
        }
        if sequence.iter().any(|num| self.is_switchable(&data, num)) {
            return Err(Error::config(
                "No accounts remain in rotation. Re-enable one with: cswap enable <num|email>",
            ));
        }
        Err(Error::config(
            "No managed accounts have valid stored credentials/config. Re-add a slot with: cswap --add-account --slot <number>",
        ))
    }

    /// `switch <num|alias|email>`. Disabled slots are allowed when named.
    pub fn switch_to(
        &self,
        identifier: &str,
        json: bool,
        force: bool,
        console: &mut dyn Console,
    ) -> Result<Option<SwitchReport>> {
        const STRATEGY: &str = "direct";
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        store::read_sequence_migrated(self.paths)?;

        let mut identifier = identifier.to_string();
        if !is_digits(&identifier) {
            let data = self.roster()?;
            let is_alias = store::find_account_by_alias(&data, &identifier).is_some();
            if !is_alias && !is_valid_email(&identifier) {
                return Err(Error::new(
                    ErrorKind::ValidationError,
                    format!("Invalid account identifier: {identifier}"),
                ));
            }
            if !json && !is_alias {
                match self.choose_among_same_email(&data, &identifier, console) {
                    Choice::Unambiguous => {}
                    Choice::Picked(num) => identifier = num,
                    Choice::Cancelled => return Ok(None),
                }
            }
        }

        let data = self.roster()?;
        let target = resolve_identifier(&data, &identifier)?.ok_or_else(|| {
            Error::new(
                ErrorKind::AccountNotFoundError,
                format!("No account found with identifier: {identifier}"),
            )
        })?;
        let Some(record) = store::accounts(&data).and_then(|a| a.get(&target)) else {
            return Err(Error::new(
                ErrorKind::AccountNotFoundError,
                format!("Account-{target} does not exist"),
            ));
        };

        let mut provenance = None;
        let live_slot = store::current_identity(self.paths)
            .filter(|_| !force)
            .and_then(|i| {
                store::find_account_slot(&data, &i.email, &i.organization_uuid)
                    .filter(|slot| *slot == target)
                    .map(|_| i.email)
            });
        if let Some(live_email) = &live_slot {
            provenance = self.self_switch_reconciliation(&target, live_email);
        }
        if live_slot.is_some() && provenance.is_none() {
            let email = record
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !json {
                console.line(&format!(
                    "{} Account-{target} ({email})",
                    self.style.accent("Already on")
                ));
                console.line(&self.style.dimmed(&format!(
                    "To rewrite the live login from the stored backup (e.g. after --import), run: cswap --switch-to {target} --force"
                )));
                return Ok(None);
            }
            let here = AccountRef::slot(&target, email);
            return Ok(Some(SwitchReport::noop(
                STRATEGY,
                "already-active",
                Some(here),
                format!("Already on Account-{target} ({email})"),
                Vec::new(),
            )));
        }

        let performed = self.perform(&target, !json, force, provenance, console)?;
        if !json {
            return Ok(None);
        }
        let mut report = SwitchReport::from_performed(performed, STRATEGY, Vec::new());
        // A forced self-activation did rewrite the live login, which
        // "already-active" would misdescribe.
        if force
            && !report.switched
            && let Some(to) = &report.to
        {
            report.reason = "activated";
            report.message = format!(
                "Activated Account-{} ({}) from stored backup",
                to.number.map_or("None".into(), |n| n.to_string()),
                to.email
            );
        }
        Ok(Some(report))
    }

    fn choose_among_same_email(
        &self,
        data: &JsonObject,
        email: &str,
        console: &mut dyn Console,
    ) -> Choice {
        let matches: Vec<(&String, &Value)> = store::accounts(data)
            .into_iter()
            .flatten()
            .filter(|(_, r)| r.get("email").and_then(Value::as_str) == Some(email))
            .collect();
        if matches.len() < 2 {
            return Choice::Unambiguous;
        }
        console.line(&format!("Multiple accounts found for '{email}':"));
        for (num, record) in &matches {
            let org = record
                .get("organizationName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            console.line(&format!(
                "  {num}: {email} {}",
                self.style
                    .muted(&format!("[{}]", crate::accounts::display_tag(org)))
            ));
        }
        let choice = console
            .prompt("Enter account number to switch to: ")
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        if is_digits(&choice) && matches.iter().any(|(num, _)| **num == choice) {
            Choice::Picked(choice)
        } else {
            console.line(&self.style.dimmed("Cancelled"));
            Choice::Cancelled
        }
    }

    /// The switch transaction. Human output is printed only when `emit`.
    /// `force` activates the target's stored login without backing up the
    /// live one; `provenance` is a pre-lock resolution already made.
    fn perform(
        &self,
        target: &str,
        emit: bool,
        force: bool,
        provenance: Option<Provenance>,
        console: &mut dyn Console,
    ) -> Result<Performed> {
        self.accounts().refuse_session_shell()?;
        let mut warnings = Vec::new();
        let provenance = match provenance {
            Some(p) => p,
            None if force => Provenance::default(),
            None => self.prefetch_live_identity(),
        };

        let (to, from, target_org, backend, direct) = {
            let _locks = self.lock()?;
            let mut data = self.roster()?;
            let target_email = store::accounts(&data)
                .and_then(|a| a.get(target))
                .and_then(|r| r.get("email"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::AccountNotFoundError,
                        format!("Account-{target} does not exist"),
                    )
                })?;
            let to = AccountRef::slot(target, &target_email);
            let target_org = record_text(&data, target, "organizationUuid").to_string();
            let identity = store::current_identity(self.paths);
            let current_account = match &identity {
                Some(i) => store::find_account_slot(&data, &i.email, &i.organization_uuid),
                None => data
                    .get("activeAccountNumber")
                    .filter(|v| !v.is_null())
                    .map(python_str),
            };

            match (&identity, current_account) {
                (Some(identity), Some(current_account)) if !force => {
                    let from = AccountRef::slot(&current_account, &identity.email);
                    let backend = self.switch_normally(
                        &mut data,
                        (&current_account, &identity.email),
                        (target, &target_email),
                        &provenance,
                        emit,
                        &mut warnings,
                        console,
                    )?;
                    (to, Some(from), target_org, backend, false)
                }
                (identity, current_account) => {
                    let from = identity.as_ref().map(|i| AccountRef {
                        number: current_account.as_deref().and_then(|n| n.parse().ok()),
                        email: i.email.clone(),
                    });
                    let backend = self.activate_directly(
                        &mut data,
                        current_account.as_deref(),
                        identity.is_some(),
                        (target, &target_email),
                        force,
                        emit,
                        &mut warnings,
                        console,
                    )?;
                    (to, from, target_org, backend, true)
                }
            }
        };

        if emit {
            let verb = if direct { "Activated" } else { "Switched to" };
            console.line(&format!(
                "{} Account-{target} ({})",
                self.style.accent(verb),
                to.email
            ));
            if !direct
                && let Some(listing) = self.listing
                && let Err(e) = listing(console)
            {
                log::warning(format!("Post-switch usage display failed: {e}"));
                console.line(
                    &self
                        .style
                        .dimmed("  (usage display unavailable — run `cswap --list` to retry)"),
                );
            }
            console.line("");
            console.line(&self.style.dimmed(followup(backend)));
            console.line("");
        }
        crate::usage_store::UsageStore::new(&self.paths.usage_cache_dir())
            .replan_new_active(target, &(to.email.clone(), target_org));
        Ok(Performed { from, to, warnings })
    }

    /// The normal path: back up (or stash) the outgoing login, then the
    /// recorded steps, rolled back newest first when a later one fails.
    #[allow(clippy::too_many_arguments)]
    fn switch_normally(
        &self,
        data: &mut JsonObject,
        (current_account, current_email): (&str, &str),
        (target, target_email): (&str, &str),
        provenance: &Provenance,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<ActiveBackend> {
        let config_path = self.paths.global_config_path();
        let original_credentials = match self.secrets.read_active().value {
            None => {
                return Err(Error::new(
                    ErrorKind::CredentialReadError,
                    "Failed to read current credentials",
                ));
            }
            Some(c) if c.is_empty() => {
                return Err(Error::new(
                    ErrorKind::CredentialReadError,
                    "Current account credential is empty (Keychain unreadable?); refusing to overwrite its backup",
                ));
            }
            Some(c) => c,
        };
        let original_config = read_live_config(&config_path)?;
        let mut transaction = SwitchTransaction {
            original_credentials,
            original_config,
            original_account: current_account.to_string(),
            config_path,
            completed: Vec::new(),
        };

        let outcome = self.run_steps(
            &mut transaction,
            data,
            (current_account, current_email),
            (target, target_email),
            provenance,
            emit,
            warnings,
            console,
        );
        if let Err(e) = &outcome {
            log::error(format!("Switch failed: {e}, attempting rollback"));
        }
        match outcome {
            Ok(backend) => Ok(backend),
            Err(e) if transaction.completed.is_empty() => Err(e),
            Err(e) => {
                let message = if transaction.rollback(self) {
                    log::info("Rollback successful");
                    format!("Switch failed and was rolled back: {e}")
                } else {
                    log::error("Rollback failed!");
                    format!(
                        "Switch failed and rollback also failed: {e}. Manual recovery may be needed."
                    )
                };
                Err(Error::new(ErrorKind::SwitchError, message))
            }
        }
    }

    /// Steps 1-5 of the normal path; every mutation of the live state is
    /// recorded in `transaction` as soon as it lands.
    #[allow(clippy::too_many_arguments)]
    fn run_steps(
        &self,
        transaction: &mut SwitchTransaction,
        data: &mut JsonObject,
        (current_account, current_email): (&str, &str),
        (target, target_email): (&str, &str),
        provenance: &Provenance,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<ActiveBackend> {
        let live = transaction.original_credentials.clone();
        let config_backup = |switcher: &Self| {
            store::write_account_config(
                switcher.paths,
                current_account,
                current_email,
                &transaction.original_config,
            )
        };
        let outgoing =
            self.classify_outgoing(current_account, current_email, &live, provenance, data);
        if let Some(reason) = outgoing.stash_reason() {
            self.stash_live_credential(
                &live,
                reason,
                current_account,
                provenance.resolved.as_ref(),
            )?;
        }
        match &outgoing {
            Outgoing::OwnBytes => {
                config_backup(self)?;
                log::info(format!(
                    "Backed up account {current_account} (config only; credentials unchanged)"
                ));
            }
            Outgoing::Wiped => {
                config_backup(self)?;
                self.warn(
                    format!(
                        "The live credential's tokens were wiped (Claude Code clears them when a refresh is rejected). Account-{current_account}'s stored backup was kept. If the account cannot authenticate after switching back, log in with Claude Code and run: cswap add"
                    ),
                    emit,
                    warnings,
                    console,
                );
            }
            Outgoing::OwnFamily | Outgoing::OwnRotated | Outgoing::Unresolved => {
                self.secrets
                    .write_backup(current_account, current_email, &live)?;
                config_backup(self)?;
                if outgoing == Outgoing::OwnRotated {
                    self.backfill_slot_uuid(data, current_account, provenance);
                }
                log::info(if outgoing == Outgoing::Unresolved {
                    format!(
                        "Backed up account {current_account} (lineage differs from the stored backup and ownership could not be verified — pre-fix backup)"
                    )
                } else {
                    format!("Backed up account {current_account}")
                });
            }
            Outgoing::Alien => self.warn(
                format!(
                    "The live login does not match a managed account. It was preserved and not written into Account-{current_account}. If you need that account, log in as it and run: cswap add"
                ),
                emit,
                warnings,
                console,
            ),
            Outgoing::Foreign(slot) => self.warn(
                format!(
                    "Credential ownership mismatch detected. The live credential was preserved and was not written into Account-{current_account}. If Account-{slot} later cannot authenticate, log in as it and run: cswap add --slot {slot}"
                ),
                emit,
                warnings,
                console,
            ),
            Outgoing::ForeignSynced(slot) => self.warn(
                format!(
                    "Credential ownership mismatch detected. The live credential already matches Account-{slot}'s stored backup, so nothing was written into Account-{current_account}."
                ),
                emit,
                warnings,
                console,
            ),
        }

        let target_credentials = self.read_target_credentials(target, target_email)?;
        let target_config = store::read_account_config(self.paths, target, target_email);
        if target_config.is_empty() {
            return Err(no_config_backup(target));
        }

        let backend = self
            .secrets
            .write_active(&prepare_credentials_for_activation(
                &target_credentials,
                Some(&live),
            ))?;
        transaction.record(Step::CredentialsWritten);
        log::info("Wrote target credentials");

        let target_config: Value =
            serde_json::from_str(&target_config).map_err(|e| Error::config(e.to_string()))?;
        let oauth_account = target_oauth_account(&target_config)?;
        self.splice_live_config(
            &transaction.config_path,
            oauth_account,
            &target_config,
            emit,
            warnings,
            console,
        )?;
        transaction.record(Step::ConfigWritten);
        log::info("Updated config file");

        record_active(self.paths, data, target)?;
        transaction.record(Step::SequenceUpdated);
        log::info(format!(
            "Switched from account {current_account} to {target}"
        ));
        Ok(backend)
    }

    /// The profile call proved the identity; a slot without a uuid (an
    /// add-token placeholder) gets it while the roster is rewritten anyway.
    fn backfill_slot_uuid(&self, data: &mut JsonObject, slot: &str, provenance: &Provenance) {
        let Some(resolved) = provenance.resolved.as_ref().filter(|r| !r.uuid.is_empty()) else {
            return;
        };
        if let Some(record) = data
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .and_then(|a| a.get_mut(slot))
            .and_then(Value::as_object_mut)
            && !record.get("uuid").is_some_and(truthy)
        {
            record.insert("uuid".into(), resolved.uuid.clone().into());
        }
    }

    /// Fresh machine, unmanaged live login or `--force`: the target's stored
    /// login replaces the live one with no backup step, so a live credential
    /// that differs from it is stashed first.
    #[allow(clippy::too_many_arguments)]
    fn activate_directly(
        &self,
        data: &mut JsonObject,
        current_account: Option<&str>,
        has_identity: bool,
        (target, target_email): (&str, &str),
        force: bool,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<ActiveBackend> {
        let target_credentials = self.read_target_credentials(target, target_email)?;
        let target_config = store::read_account_config(self.paths, target, target_email);
        if target_config.is_empty() {
            return Err(no_config_backup(target));
        }
        let target_config: Value = serde_json::from_str(&target_config).map_err(|e| {
            Error::new(
                ErrorKind::SwitchError,
                format!("Invalid backup config: {e}"),
            )
        })?;
        let oauth_account = target_oauth_account(&target_config)?;

        let Some(live) = self.secrets.read_active().value else {
            return Err(Error::new(
                ErrorKind::CredentialReadError,
                "Cannot snapshot live credentials before activation",
            ));
        };
        let rollback_credentials = (has_identity || !live.is_empty()).then_some(live);
        let config_path = self.paths.global_config_path();
        let rollback_config = if config_path.exists() {
            Some(std::fs::read_to_string(&config_path).map_err(|e| {
                Error::config(format!(
                    "Cannot snapshot live config before activation: {e}"
                ))
            })?)
        } else {
            None
        };

        if let Some(live) = rollback_credentials
            .as_deref()
            .filter(|l| !l.is_empty() && *l != target_credentials)
            && let Err(e) = self.stash_live_credential(
                live,
                "displaced-live-login",
                current_account.unwrap_or("unmanaged"),
                None,
            )
        {
            if !force {
                return Err(Error::new(
                    ErrorKind::SwitchError,
                    format!(
                        "Could not preserve the live credential before activation (safety-copy write failed: {e}); aborting rather than destroying it"
                    ),
                ));
            }
            self.warn(
                format!(
                    "Could not preserve the replaced live credential (safety-copy write failed: {e}) — proceeding because --force explicitly rewrites the live login."
                ),
                emit,
                warnings,
                console,
            );
        }

        let mut completed = Vec::new();
        let outcome = (|| -> Result<ActiveBackend> {
            let backend = self
                .secrets
                .write_active(&prepare_credentials_for_activation(
                    &target_credentials,
                    rollback_credentials.as_deref(),
                ))?;
            completed.push(Step::CredentialsWritten);
            self.splice_live_config(
                &config_path,
                oauth_account,
                &target_config,
                emit,
                warnings,
                console,
            )?;
            completed.push(Step::ConfigWritten);
            record_active(self.paths, data, target)?;
            Ok(backend)
        })();
        if outcome.is_err() {
            if completed.contains(&Step::ConfigWritten)
                && let Some(text) = &rollback_config
            {
                let _ = std::fs::write(&config_path, text)
                    .and_then(|()| fsutil::chmod(&config_path, 0o600));
            }
            if completed.contains(&Step::CredentialsWritten)
                && let Some(credentials) = &rollback_credentials
            {
                let _ = self.secrets.write_active(credentials);
            }
        }
        outcome
    }

    /// Puts the target's `oauthAccount` into the live config, keeping every
    /// other key; an unparseable config is copied aside and replaced by the
    /// target's whole backup config.
    fn splice_live_config(
        &self,
        config_path: &Path,
        oauth_account: Value,
        target_config: &Value,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<()> {
        match store::read_json_lenient(config_path) {
            Some(mut live_config) => {
                live_config.insert("oauthAccount".into(), oauth_account);
                fsutil::write_json_pid_tmp(config_path, &Value::Object(live_config))
            }
            None => {
                if config_path.exists() {
                    self.salvage_unreadable(config_path, emit, warnings, console)?;
                }
                fsutil::write_json_pid_tmp(config_path, target_config)
            }
        }
    }

    /// Ownership of the live bytes leaving `current`. Only a resolution of
    /// exactly these bytes counts, and only a complete or uuid-positive one
    /// may keep them out of the slot; anything weaker falls open.
    fn classify_outgoing(
        &self,
        current: &str,
        current_email: &str,
        live: &str,
        provenance: &Provenance,
        data: &JsonObject,
    ) -> Outgoing {
        let backup = self.secrets.read_backup(current, current_email).value;
        if !backup.is_empty() && backup == live {
            return Outgoing::OwnBytes;
        }
        if !backup.is_empty() && same_lineage(&backup, live) {
            return Outgoing::OwnFamily;
        }
        if let Some(oauth) = anthropic_client::extract_oauth_data(live)
            && !oauth.get("accessToken").is_some_and(truthy)
            && !oauth.get("refreshToken").is_some_and(truthy)
        {
            return Outgoing::Wiped;
        }
        let Some(resolved) = provenance
            .resolved
            .as_ref()
            .filter(|_| provenance.live.as_deref() == Some(live))
        else {
            return Outgoing::Unresolved;
        };
        let r_email = resolved.email.as_deref().unwrap_or_default();
        let r_org = resolved.organization_uuid.as_deref().unwrap_or_default();
        let r_uuid = resolved.uuid.trim();
        let own_uuid = record_text(data, current, "uuid").trim();
        let own_org = record_text(data, current, "organizationUuid");
        if !r_uuid.is_empty()
            && r_uuid == own_uuid
            && (r_org.is_empty() || own_org.is_empty() || r_org == own_org)
        {
            return Outgoing::OwnRotated;
        }
        let stored_uuid = |slot: &str| record_text(data, slot, "uuid").trim().to_string();
        let mut slot = (!r_email.is_empty())
            .then(|| store::find_account_slot(data, r_email, r_org))
            .flatten();
        if let Some(s) = &slot
            && !r_uuid.is_empty()
        {
            let stored = stored_uuid(s);
            if !stored.is_empty() && stored != r_uuid {
                slot = None;
            }
        }
        if slot.is_none() && !r_uuid.is_empty() {
            slot = store::accounts(data)
                .into_iter()
                .flatten()
                .find_map(|(num, acct)| {
                    let uuid = acct.get("uuid").and_then(Value::as_str).unwrap_or_default();
                    (!uuid.is_empty()
                        && uuid == r_uuid
                        && record_text(data, num, "organizationUuid") == r_org)
                        .then(|| num.clone())
                });
        }
        let Some(slot) = slot else {
            if !r_email.is_empty() && resolved.organization_uuid.is_some() {
                return Outgoing::Alien;
            }
            return Outgoing::Unresolved;
        };
        if slot == current {
            return Outgoing::OwnRotated;
        }
        if r_uuid.is_empty() || stored_uuid(&slot) != r_uuid {
            return Outgoing::Alien;
        }
        let foreign_backup = self
            .secrets
            .read_backup(&slot, record_text(data, &slot, "email"))
            .value;
        if !foreign_backup.is_empty() && same_lineage(&foreign_backup, live) {
            return Outgoing::ForeignSynced(slot);
        }
        Outgoing::Foreign(slot)
    }

    /// Resolves the live credential's owner before any lock is taken (the
    /// only network call a switch makes). Skipped when the live bytes are
    /// provably the live slot's own; never fails.
    fn prefetch_live_identity(&self) -> Provenance {
        let mut provenance = Provenance::default();
        let Some(live) = self.secrets.read_active().value else {
            return provenance;
        };
        provenance.live = Some(live.clone());
        if live.is_empty() {
            return provenance;
        }
        let Some(identity) = store::current_identity(self.paths) else {
            return provenance;
        };
        let data = store::read_sequence(self.paths)
            .ok()
            .flatten()
            .unwrap_or_default();
        let Some(slot) =
            store::find_account_slot(&data, &identity.email, &identity.organization_uuid)
        else {
            return provenance;
        };
        let backup = self.secrets.read_backup(&slot, &identity.email).value;
        if same_lineage(&backup, &live) {
            return provenance;
        }
        let Some(token) =
            crate::secret_store::extract_access_token(&live).filter(|t| !t.is_empty())
        else {
            return provenance;
        };
        provenance.resolved = self.client.fetch_profile(&token);
        provenance
    }

    /// For a switch onto the live slot: the provenance to reconcile with
    /// when the live bytes diverged from the slot's backup and their owner
    /// was resolved; `None` (stay put) otherwise.
    fn self_switch_reconciliation(&self, slot: &str, email: &str) -> Option<Provenance> {
        if self.live_matches_slot_backup(slot, email) {
            return None;
        }
        let provenance = self.prefetch_live_identity();
        provenance.resolved.is_some().then_some(provenance)
    }

    /// Unreadable or empty live credentials count as matching: forcing a
    /// switch on missing evidence would fail later anyway.
    fn live_matches_slot_backup(&self, slot: &str, email: &str) -> bool {
        let Some(live) = self.secrets.read_active().value.filter(|l| !l.is_empty()) else {
            return true;
        };
        let backup = self.secrets.read_backup(slot, email).value;
        !backup.is_empty() && same_lineage(&live, &backup)
    }

    /// Writes the safety copy that licenses overwriting `live`.
    fn stash_live_credential(
        &self,
        live: &str,
        reason: &str,
        config_slot: &str,
        resolved: Option<&Profile>,
    ) -> Result<String> {
        let credentials_mtime = std::fs::metadata(self.paths.credentials_path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| store::format_timestamp(d.as_secs() as i64));
        let live_oauth_account = store::read_json_lenient(&self.paths.global_config_path())
            .and_then(|c| c.get("oauthAccount").cloned())
            .unwrap_or(Value::Null);
        let context = json!({
            "reason": reason,
            "configSlot": config_slot,
            "fingerprint": anthropic_client::credential_fingerprint(live),
            "liveOauthAccount": live_oauth_account,
            "resolvedIdentity": resolved.map_or(Value::Null, profile_json),
            "credentialsMtime": credentials_mtime,
        });
        let Value::Object(context) = context else {
            unreachable!("json! object literal");
        };
        UnclaimedStash::new(self.paths).write(live, context)
    }

    fn read_target_credentials(&self, num: &str, email: &str) -> Result<String> {
        let read = self.secrets.read_backup(num, email);
        if !read.value.is_empty() {
            return Ok(read.value);
        }
        let message = if read.unreadable {
            format!(
                "Account-{num}'s backup is in the macOS Keychain but it is unreadable right now (locked or no GUI session). Retry from a GUI terminal; do not re-add."
            )
        } else {
            format!(
                "Account-{num} has no stored credentials. Re-add with: cswap --add-account --slot {num}"
            )
        };
        Err(Error::new(ErrorKind::SwitchError, message))
    }

    /// Keeps the bytes of a `~/.claude.json` that cannot be parsed before
    /// it is replaced: `<name>.unreadable-<epoch>[.n]`, mode 0600.
    fn salvage_unreadable(
        &self,
        path: &Path,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<()> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let stem = format!("{name}.unreadable-{epoch}");
        let mut salvage = path.with_file_name(&stem);
        let mut n = 1;
        while salvage.exists() {
            salvage = path.with_file_name(format!("{stem}.{n}"));
            n += 1;
        }
        std::fs::copy(path, &salvage)
            .and_then(|_| fsutil::chmod(&salvage, 0o600))
            .map_err(|e| {
                Error::new(
                    ErrorKind::SwitchError,
                    format!(
                        "{} could not be parsed and the salvage copy failed ({e}); aborting rather than destroying it",
                        path.display()
                    ),
                )
            })?;
        log::warning(format!(
            "{} could not be parsed; a copy was kept at {} before it was replaced",
            path.display(),
            salvage.display()
        ));
        let message = format!(
            "{name} could not be parsed — a copy was kept at {}",
            salvage.file_name().unwrap_or_default().to_string_lossy()
        );
        warnings.push(message.clone());
        if emit {
            console.line(&self.style.yellowed(&message));
        }
        Ok(())
    }

    fn warn(
        &self,
        message: String,
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) {
        if emit {
            console.line(&self.style.yellowed(&message));
        } else {
            warnings.push(message);
        }
    }

    /// Both a stored credential and a stored config.
    fn is_switchable(&self, data: &JsonObject, num: &str) -> bool {
        let Some(record) = store::accounts(data)
            .and_then(|a| a.get(num))
            .filter(|r| truthy(r))
        else {
            return false;
        };
        let email = record
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default();
        !self.secrets.read_backup(num, email).value.is_empty()
            && !store::read_account_config(self.paths, num, email).is_empty()
    }

    fn lock(&self) -> Result<TransactionLocks> {
        let store = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let credentials = locks::claude_credentials_lock(self.paths, None)?;
        let config = locks::claude_config_lock(self.paths, None)?;
        Ok(TransactionLocks {
            _config: config,
            _credentials: credentials,
            _store: store,
        })
    }

    fn roster(&self) -> Result<JsonObject> {
        Ok(store::read_sequence(self.paths)?.unwrap_or_default())
    }
}

enum Choice {
    Unambiguous,
    Picked(String),
    Cancelled,
}

/// The credential to activate: the target slot's login with the live
/// machine-shared fields. Without a live JSON credential, or for a target
/// that is not an OAuth object, the target activates verbatim.
pub fn prepare_credentials_for_activation(target: &str, live: Option<&str>) -> String {
    let Some(live) = live.and_then(credential_object) else {
        return target.to_string();
    };
    let Some(target_object) = credential_object(target).filter(|t| t.contains_key("claudeAiOauth"))
    else {
        return target.to_string();
    };
    let mut composed: JsonObject = target_object
        .into_iter()
        .filter(|(k, _)| !SHARED_CREDENTIAL_KEYS.contains(&k.as_str()))
        .collect();
    for key in SHARED_CREDENTIAL_KEYS {
        if let Some(value) = live.get(key) {
            composed.insert(key.into(), value.clone());
        }
    }
    pyjson::to_string_compact(&Value::Object(composed))
}

fn credential_object(credentials: &str) -> Option<JsonObject> {
    if credentials.is_empty() || crate::secret_store::looks_like_api_key(credentials) {
        return None;
    }
    match serde_json::from_str(credentials).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

fn read_live_config(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::config("Claude config file not found"),
        std::io::ErrorKind::PermissionDenied => {
            Error::config("Permission denied reading Claude config")
        }
        _ => Error::config(format!("Could not read Claude config: {e}")),
    })
}

fn followup(backend: ActiveBackend) -> &'static str {
    match backend {
        ActiveBackend::Keychain => {
            "Restart Claude Code to apply immediately — otherwise the session can take up to ~30 seconds to pick up the new account."
        }
        ActiveBackend::File => "New account is active on your next message — no restart needed.",
    }
}

/// Number, then alias, then email; an email on several slots is an error.
fn resolve_identifier(data: &JsonObject, identifier: &str) -> Result<Option<String>> {
    if is_digits(identifier) {
        return Ok(Some(identifier.to_string()));
    }
    if let Some(num) = store::find_account_by_alias(data, identifier) {
        return Ok(Some(num));
    }
    let matches: Vec<(&String, &Value)> = store::accounts(data)
        .into_iter()
        .flatten()
        .filter(|(_, r)| r.get("email").and_then(Value::as_str) == Some(identifier))
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [(num, _)] => Ok(Some((*num).clone())),
        _ => {
            let details: Vec<String> = matches
                .iter()
                .map(|(num, r)| {
                    let org = r
                        .get("organizationName")
                        .and_then(Value::as_str)
                        .filter(|o| !o.is_empty())
                        .unwrap_or("personal");
                    format!("{num} [{org}]")
                })
                .collect();
            Err(Error::config(format!(
                "Email '{identifier}' is ambiguous — matches accounts: {}. Use account number instead (e.g., cswap --switch-to 1).",
                details.join(", ")
            )))
        }
    }
}

fn target_oauth_account(target_config: &Value) -> Result<Value> {
    target_config
        .get("oauthAccount")
        .filter(|v| truthy(v))
        .cloned()
        .ok_or_else(|| Error::new(ErrorKind::SwitchError, "Invalid oauthAccount in backup"))
}

fn record_active(paths: &Paths, data: &mut JsonObject, target: &str) -> Result<()> {
    let number: i64 = target.parse().map_err(|_| {
        Error::config(format!(
            "invalid literal for int() with base 10: '{target}'"
        ))
    })?;
    data.insert("activeAccountNumber".into(), number.into());
    data.insert("lastUpdated".into(), store::timestamp_now().into());
    store::write_sequence(paths, data)
}

fn no_config_backup(num: &str) -> Error {
    Error::new(
        ErrorKind::SwitchError,
        format!(
            "Account-{num} has no stored config backup. Re-add with: cswap --add-account --slot {num}"
        ),
    )
}

fn is_disabled(data: &JsonObject, num: &str) -> bool {
    store::accounts(data)
        .and_then(|a| a.get(num))
        .and_then(|r| r.get("disabled"))
        .is_some_and(truthy)
}

fn slot_number(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python's `str()` of a roster value, as slot keys are derived.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// `^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$` under `re.match`.
fn is_valid_email(text: &str) -> bool {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let Some((local, domain)) = text.split_once('@') else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation_matches_the_upstream_pattern() {
        assert!(is_valid_email("a.b+c@example.co"));
        assert!(is_valid_email("a@b.example.com\n"));
        assert!(!is_valid_email("a@example.c"));
        assert!(!is_valid_email("a@example.c0m"));
        assert!(!is_valid_email("a@.com"));
        assert!(!is_valid_email("work"));
        assert!(!is_valid_email("a@b@example.com"));
        assert!(!is_valid_email("@example.com"));
    }
}
