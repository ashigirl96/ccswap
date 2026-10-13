//! The switch: pick a target, then one transaction under the store lock and
//! Claude Code's locks that backs up the outgoing login, activates the
//! target and rolls every recorded step back if a later one fails.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::accounts::{Accounts, AddOptions, Console};
use crate::anthropic_client::{self, AnthropicClient};
use crate::env::Environment;
use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{self, STORE_LOCK_TIMEOUT, StoreLock};
use crate::paths::Paths;
use crate::presentation::{SCHEMA_VERSION, Style};
use crate::pyjson;
use crate::secret_store::{ActiveBackend, SecretStore};
use crate::store::{self, JsonObject};

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

/// Whose the live credential is, judged against the outgoing slot's backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outgoing {
    /// Byte-identical to the backup: only the config needs refreshing.
    OwnBytes,
    /// Same refresh-token lineage: back up the newer generation.
    OwnFamily,
    /// Claude Code emptied the tokens after a rejected refresh; writing
    /// them would destroy the slot's only refresh token.
    Wiped,
    /// Lineage differs and the owner is unknown: back up (fail open).
    Unresolved,
}

fn classify_outgoing(backup: &str, live: &str) -> Outgoing {
    if !backup.is_empty() && backup == live {
        return Outgoing::OwnBytes;
    }
    if !backup.is_empty()
        && anthropic_client::credential_fingerprint(backup)
            == anthropic_client::credential_fingerprint(live)
    {
        return Outgoing::OwnFamily;
    }
    if let Some(oauth) = anthropic_client::extract_oauth_data(live)
        && !oauth.get("accessToken").is_some_and(truthy)
        && !oauth.get("refreshToken").is_some_and(truthy)
    {
        return Outgoing::Wiped;
    }
    Outgoing::Unresolved
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    CredentialsWritten,
    ConfigWritten,
    SequenceUpdated,
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
            ok &= undone.is_ok();
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
    /// after the recorded active one.
    pub fn switch_next(
        &self,
        json: bool,
        console: &mut dyn Console,
    ) -> Result<Option<SwitchReport>> {
        const STRATEGY: &str = "rotation";
        let style = self.style;
        let mut warnings = Vec::new();
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        let identity = store::current_identity(self.paths);
        store::read_sequence_migrated(self.paths)?;

        let Some(identity) = identity else {
            return Err(not_yet(
                "switching without a live Claude login (fresh-machine activation)",
            ));
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
                    STRATEGY,
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
                    STRATEGY,
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
        let active = data.get("activeAccountNumber");
        let start = active
            .and_then(slot_number)
            .and_then(|n| sequence.iter().position(|v| v.as_i64() == Some(n)))
            .unwrap_or(0);

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
            next = Some(candidate);
            break;
        }

        let Some(next) = next else {
            if json {
                return Ok(Some(SwitchReport::noop(
                    STRATEGY,
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

        if next == live_slot {
            let message = format!("Already on Account-{next} ({email})");
            if json {
                return Ok(Some(SwitchReport::noop(
                    STRATEGY,
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

        let performed = self.perform(&next, !json, console)?;
        Ok(json.then(|| SwitchReport::from_performed(performed, STRATEGY, warnings)))
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
        if force {
            return Err(not_yet("switch --force"));
        }

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

        if let Some(identity) = store::current_identity(self.paths)
            && store::find_account_slot(&data, &identity.email, &identity.organization_uuid)
                .as_deref()
                == Some(target.as_str())
        {
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

        let performed = self.perform(&target, !json, console)?;
        Ok(json.then(|| SwitchReport::from_performed(performed, STRATEGY, Vec::new())))
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
    fn perform(&self, target: &str, emit: bool, console: &mut dyn Console) -> Result<Performed> {
        self.accounts().refuse_session_shell()?;
        let mut warnings = Vec::new();

        let (performed_to, from, target_email, target_org, backend) = {
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
            let identity = store::current_identity(self.paths);
            let current = identity.as_ref().and_then(|i| {
                store::find_account_slot(&data, &i.email, &i.organization_uuid)
                    .map(|slot| (slot, i.email.clone()))
            });
            let Some((current_account, current_email)) = current else {
                return Err(not_yet(
                    "switching from an unmanaged or absent live login (direct activation)",
                ));
            };
            let from = AccountRef::slot(&current_account, &current_email);

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
                original_account: current_account.clone(),
                config_path,
                completed: Vec::new(),
            };

            let outcome = self.run_steps(
                &mut transaction,
                &mut data,
                (&current_account, &current_email),
                (target, &target_email),
                emit,
                &mut warnings,
                console,
            );
            let backend = match outcome {
                Ok(backend) => backend,
                Err(e) if transaction.completed.is_empty() => return Err(e),
                Err(e) => {
                    let message = if transaction.rollback(self) {
                        format!("Switch failed and was rolled back: {e}")
                    } else {
                        format!(
                            "Switch failed and rollback also failed: {e}. Manual recovery may be needed."
                        )
                    };
                    return Err(Error::new(ErrorKind::SwitchError, message));
                }
            };
            let target_org = store::accounts(&data)
                .and_then(|a| a.get(target))
                .and_then(|r| r.get("organizationUuid"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            (to, from, target_email, target_org, backend)
        };

        if emit {
            console.line(&format!(
                "{} Account-{target} ({target_email})",
                self.style.accent("Switched to")
            ));
            if let Some(listing) = self.listing
                && listing(console).is_err()
            {
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
            .replan_new_active(target, &(target_email.clone(), target_org));
        Ok(Performed {
            from: Some(from),
            to: performed_to,
            warnings,
        })
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
        emit: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<ActiveBackend> {
        let live = transaction.original_credentials.clone();
        let backup = self
            .secrets
            .read_backup(current_account, current_email)
            .value;
        match classify_outgoing(&backup, &live) {
            Outgoing::OwnBytes => {
                store::write_account_config(
                    self.paths,
                    current_account,
                    current_email,
                    &transaction.original_config,
                )?;
            }
            Outgoing::Wiped => {
                store::write_account_config(
                    self.paths,
                    current_account,
                    current_email,
                    &transaction.original_config,
                )?;
                let message = format!(
                    "The live credential's tokens were wiped (Claude Code clears them when a refresh is rejected). Account-{current_account}'s stored backup was kept. If the account cannot authenticate after switching back, log in with Claude Code and run: cswap add"
                );
                self.warn(message, emit, warnings, console);
            }
            Outgoing::OwnFamily | Outgoing::Unresolved => {
                self.secrets
                    .write_backup(current_account, current_email, &live)?;
                store::write_account_config(
                    self.paths,
                    current_account,
                    current_email,
                    &transaction.original_config,
                )?;
            }
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

        let target_config: Value =
            serde_json::from_str(&target_config).map_err(|e| Error::config(e.to_string()))?;
        let oauth_account = target_config
            .get("oauthAccount")
            .filter(|v| truthy(v))
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::SwitchError, "Invalid oauthAccount in backup"))?;
        let config_path = &transaction.config_path;
        match store::read_json_lenient(config_path) {
            Some(mut live_config) => {
                live_config.insert("oauthAccount".into(), oauth_account);
                fsutil::write_json_pid_tmp(config_path, &Value::Object(live_config))?;
            }
            None => {
                if config_path.exists() {
                    self.salvage_unreadable(config_path, emit, warnings, console)?;
                }
                fsutil::write_json_pid_tmp(config_path, &target_config)?;
            }
        }
        transaction.record(Step::ConfigWritten);

        let number: i64 = target.parse().map_err(|_| {
            Error::config(format!(
                "invalid literal for int() with base 10: '{target}'"
            ))
        })?;
        data.insert("activeAccountNumber".into(), number.into());
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.paths, data)?;
        transaction.record(Step::SequenceUpdated);
        Ok(backend)
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

fn no_config_backup(num: &str) -> Error {
    Error::new(
        ErrorKind::SwitchError,
        format!(
            "Account-{num} has no stored config backup. Re-add with: cswap --add-account --slot {num}"
        ),
    )
}

fn not_yet(what: &str) -> Error {
    Error::new(
        ErrorKind::ClaudeSwitchError,
        format!("{what} is not implemented yet in ccswap"),
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
