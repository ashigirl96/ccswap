//! Account management on top of Store, SecretStore and the profile
//! endpoint. `add` captures the live login into a slot.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::anthropic_client::{self, AnthropicClient};
use crate::env::Environment;
use crate::error::{Error, ErrorKind, Result};
use crate::locks::{self, STORE_LOCK_TIMEOUT, StoreLock};
use crate::mappings::MappingStore;
use crate::paths::Paths;
use crate::presentation::Style;
use crate::secret_store::{SecretStore, looks_like_api_key};
use crate::store::{self, Identity, JsonObject};
use crate::usage_store::UsageStore;

mod token;
pub use token::{AddTokenOptions, TokenInput, getpass};

/// Where account operations print and ask. The CLI writes to the terminal;
/// the TUI and menu bar collect lines and confirm on their own.
pub trait Console {
    fn line(&mut self, text: &str);
    /// Shows `question` and returns the reply, `None` on end of input.
    fn prompt(&mut self, question: &str) -> Option<String>;
    /// Like `prompt`, without echoing the reply.
    fn secret(&mut self, question: &str) -> Option<String> {
        self.prompt(question)
    }
}

pub struct Accounts<'a> {
    pub paths: &'a Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
    pub style: &'a Style,
}

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub slot: Option<i64>,
    pub alias: Option<String>,
    /// Skip the overwrite prompt (callers that confirmed already).
    pub assume_yes: bool,
}

/// What `add` decided before touching anything.
struct Plan {
    num: String,
    displace: Option<(String, String, String)>,
    migrate_from: Option<String>,
    carried_alias: Option<String>,
}

/// The store `.lock`, then Claude Code's credential and config locks, held
/// for the local re-checks and writes of a registration, never across the
/// network.
struct CommitLocks {
    _config: locks::DirLock,
    _credentials: locks::ClaudeCredentialsLock,
    _store: StoreLock,
}

impl Accounts<'_> {
    pub fn add_account(&self, options: &AddOptions, console: &mut dyn Console) -> Result<()> {
        self.refuse_session_shell()?;
        store::setup_directories(self.paths)?;
        store::init_sequence(self.paths)?;
        store::migrate_org_fields(self.paths)?;

        let alias = options
            .alias
            .as_deref()
            .map(store::normalize_alias)
            .transpose()?;
        let identity = store::current_identity(self.paths)
            .ok_or_else(|| Error::config("No active Claude account found. Please log in first."))?;
        let data = self.roster()?;

        if options.slot.is_none()
            && let Some(num) =
                store::find_account_slot(&data, &identity.email, &identity.organization_uuid)
        {
            return self.refresh_in_place(&num, &data, &identity, alias, console);
        }

        let Some(plan) = self.plan_new_slot(options, &data, &identity, console)? else {
            return Ok(());
        };
        if let Some(alias) = &alias {
            reject_alias_in_use(&data, alias, &plan.num)?;
        }
        let creds = self.capture_verified(&identity, console)?;

        let _locks = self.commit_locks()?;
        self.reject_credential_drift(&creds)?;
        let config_text = self.read_live_config_text()?;
        let live = store::read_json_lenient(&self.paths.global_config_path())
            .and_then(|c| c.get("oauthAccount").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        let field = |key: &str| {
            live.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let (account_uuid, org_uuid, org_name) = (
            field("accountUuid"),
            field("organizationUuid"),
            field("organizationName"),
        );
        self.reject_identity_drift(&identity)?;

        if let Some((num, email, org)) = &plan.displace {
            self.delete_account_files(num, email);
            self.drop_record(num)?;
            self.prune_mappings(email, org, console);
        }
        if let Some(old) = &plan.migrate_from {
            let data = self.roster()?;
            let old_email = record_text(&data, old, "email");
            self.delete_account_files(old, &old_email);
            self.drop_record(old)?;
        }

        let email = &identity.email;
        self.secrets.write_backup(&plan.num, email, &creds)?;
        store::write_account_config(self.paths, &plan.num, email, &config_text)?;
        self.clear_dead_token(&plan.num, email, &org_uuid)?;

        let mut data = self.roster()?;
        let mut record = json!({
            "email": email,
            "uuid": account_uuid,
            "organizationUuid": org_uuid,
            "organizationName": org_name,
            "added": store::timestamp_now(),
        });
        if let Some(carried) = alias.or(plan.carried_alias).filter(|a| !a.is_empty()) {
            record["alias"] = carried.into();
        }
        let num_int: i64 = plan.num.parse().unwrap_or_default();
        accounts_mut(&mut data).insert(plan.num.clone(), record);
        let sequence = sequence_mut(&mut data);
        if !sequence.iter().any(|v| v.as_i64() == Some(num_int)) {
            sequence.push(num_int.into());
            sequence.sort_by_key(|v| v.as_i64().unwrap_or_default());
        }
        data.insert("activeAccountNumber".into(), num_int.into());
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.paths, &data)?;

        let style = self.style;
        if let Some(old) = &plan.migrate_from {
            console.line(&style.dimmed(&format!(
                "Moved from slot {old} → {}",
                options.slot.unwrap_or_default()
            )));
        }
        console.line(&format!(
            "{} Account {}: {email} {}",
            style.accent("Added"),
            plan.num,
            style.muted(&format!("[{}]", display_tag(&org_name)))
        ));
        Ok(())
    }

    fn refresh_in_place(
        &self,
        num: &str,
        data: &JsonObject,
        identity: &Identity,
        alias: Option<String>,
        console: &mut dyn Console,
    ) -> Result<()> {
        let org_name = record_text(data, num, "organizationName");
        if let Some(alias) = &alias {
            reject_alias_in_use(data, alias, num)?;
        }
        let creds = self.capture_verified(identity, console)?;

        let _locks = self.commit_locks()?;
        self.reject_credential_drift(&creds)?;
        let config_text = self.read_live_config_text()?;
        self.reject_identity_drift(identity)?;

        self.secrets.write_backup(num, &identity.email, &creds)?;
        store::write_account_config(self.paths, num, &identity.email, &config_text)?;
        self.clear_dead_token(num, &identity.email, &identity.organization_uuid)?;

        let mut data = self.roster()?;
        if let Some(alias) = alias
            && let Some(record) = accounts_mut(&mut data)
                .get_mut(num)
                .and_then(Value::as_object_mut)
        {
            record.insert("alias".into(), alias.into());
        }
        data.insert(
            "activeAccountNumber".into(),
            num.parse::<i64>().unwrap_or_default().into(),
        );
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.paths, &data)?;

        let style = self.style;
        console.line(&format!(
            "{} for Account {num} ({} {}).",
            style.accent("Updated credentials"),
            identity.email,
            style.muted(&format!("[{}]", display_tag(&org_name)))
        ));
        Ok(())
    }

    /// Slot choice, overwrite confirmation and alias carry-over. `None` when
    /// the user declined to overwrite.
    fn plan_new_slot(
        &self,
        options: &AddOptions,
        data: &JsonObject,
        identity: &Identity,
        console: &mut dyn Console,
    ) -> Result<Option<Plan>> {
        let Some(slot) = options.slot else {
            return Ok(Some(Plan {
                num: store::next_account_number(Some(data)).to_string(),
                displace: None,
                migrate_from: None,
                carried_alias: None,
            }));
        };
        if slot < 1 {
            return Err(Error::config("Slot number must be >= 1"));
        }
        let num = slot.to_string();
        let migrate_from =
            store::find_account_slot(data, &identity.email, &identity.organization_uuid)
                .filter(|old| *old != num);

        let mut displace = None;
        let mut carried_alias = None;
        if let Some(existing) = store::accounts(data).and_then(|a| a.get(&num)) {
            let existing_email = existing
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let existing_org = existing
                .get("organizationUuid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if existing_email == identity.email && existing_org == identity.organization_uuid {
                carried_alias = existing
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            } else {
                let style = self.style;
                let existing_name = existing
                    .get("organizationName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                console.line(&style.yellowed(&format!("Slot {slot} already occupied")));
                console.line(&format!(
                    "{existing_email} {}",
                    style.muted(&format!("[{}]", display_tag(existing_name)))
                ));
                if !options.assume_yes {
                    match console.prompt(&format!("Overwrite slot {slot}? [y/N] ")) {
                        None => {
                            console.line(&format!("\n{}", style.dimmed("Cancelled")));
                            return Ok(None);
                        }
                        Some(answer)
                            if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") => {}
                        Some(_) => {
                            console.line(&style.dimmed("Cancelled"));
                            return Ok(None);
                        }
                    }
                }
                displace = Some((num.clone(), existing_email, existing_org));
            }
        }
        if let Some(old) = &migrate_from {
            let old_alias = store::accounts(data)
                .and_then(|a| a.get(old))
                .and_then(|r| r.get("alias"))
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty())
                .map(str::to_string);
            carried_alias = old_alias.or(carried_alias);
        }
        Ok(Some(Plan {
            num,
            displace,
            migrate_from,
            carried_alias,
        }))
    }

    /// Read the live credential and refuse it unless it is the logged-in
    /// account's own (advisory: an unresolvable owner registers with a notice).
    fn capture_verified(&self, identity: &Identity, console: &mut dyn Console) -> Result<String> {
        let creds = match self.secrets.read_capture()? {
            None => {
                return Err(Error::new(
                    ErrorKind::CredentialReadError,
                    "Failed to read credentials for current account",
                ));
            }
            Some(c) if c.is_empty() => {
                return Err(Error::new(
                    ErrorKind::CredentialReadError,
                    "No credentials found for current account",
                ));
            }
            Some(c) => c,
        };
        if looks_like_api_key(&creds) {
            return Err(Error::new(
                ErrorKind::ValidationError,
                "Active login is an API-key account. Add it with 'cswap --add-token sk-ant-api...' instead of --add-account.",
            ));
        }
        self.reject_foreign_credential(&creds, identity, console)?;
        Ok(creds)
    }

    fn reject_foreign_credential(
        &self,
        creds: &str,
        identity: &Identity,
        console: &mut dyn Console,
    ) -> Result<()> {
        let email = &identity.email;
        let mut unverified = |why: &str| {
            console.line(&format!(
                "{} could not verify that the stored credential belongs to {email} ({why}). Registering anyway; re-run where the check can complete to confirm.",
                self.style.accent("Notice:")
            ));
            Ok(())
        };
        let Some(token) =
            crate::secret_store::extract_access_token(creds).filter(|t| !t.is_empty())
        else {
            return unverified("no access token to resolve");
        };
        if let Some(oauth) = anthropic_client::extract_oauth_data(creds)
            && anthropic_client::is_oauth_token_expired(oauth.get("expiresAt"))
        {
            return unverified("the access token is expired");
        }
        let Some(profile) = self.client.fetch_profile(&token) else {
            return unverified("the identity lookup did not resolve");
        };
        let foreign = |resolved: &str| {
            Err(Error::config(format!(
                "The stored credential does not belong to {email}: the token resolves to {resolved}. Nothing was changed. This happens when the config names one account while the credential store still holds another's token (e.g. a renamed .claude.json over a live keychain item). Log in as {email} in THIS environment, then re-run."
            )))
        };
        if !identity.account_uuid.is_empty() {
            if profile.uuid != identity.account_uuid {
                return foreign(&format!(
                    "account {}, not {}",
                    profile.uuid, identity.account_uuid
                ));
            }
        } else {
            let seen = profile.email.as_deref().unwrap_or_default().trim();
            if seen.is_empty() {
                return unverified("the resolved identity carries no address");
            }
            if seen.to_lowercase() != email.to_lowercase() {
                return foreign(seen);
            }
        }
        let Some(resolved_org) = profile.organization_uuid else {
            return Ok(());
        };
        let seen_org = resolved_org.trim();
        let org = &identity.organization_uuid;
        if seen_org == org {
            return Ok(());
        }
        let name = |o: &str| {
            if o.is_empty() {
                "personal".to_string()
            } else {
                o.to_string()
            }
        };
        Err(Error::config(format!(
            "The stored credential for {email} belongs to organization {}, not {}. Nothing was changed. Two accounts can share an email across organizations. Log in as {email} in the {} organization in THIS environment, then re-run.",
            name(seen_org),
            name(org),
            name(org)
        )))
    }

    /// A refresh of the same account during verification moves only the
    /// credential; storing the verified bytes would file a retired generation.
    fn reject_credential_drift(&self, verified: &str) -> Result<()> {
        let Ok(Some(now)) = self.secrets.read_capture() else {
            return Ok(());
        };
        if now.is_empty() {
            return Ok(());
        }
        match (
            anthropic_client::credential_fingerprint(verified),
            anthropic_client::credential_fingerprint(&now),
        ) {
            (Some(before), Some(after)) if before != after => Err(Error::config(
                "The stored credential rotated while it was being verified. Nothing was changed. Registering the pre-rotation generation would hand the slot a credential the server has already retired. Re-run when no refresh is in flight.",
            )),
            _ => Ok(()),
        }
    }

    /// The identity that was verified must still be the one logged in.
    fn reject_identity_drift(&self, verified: &Identity) -> Result<()> {
        let now = store::current_identity(self.paths);
        if now.as_ref() == Some(verified) {
            return Ok(());
        }
        let now_email = now.map(|i| i.email).filter(|e| !e.is_empty());
        Err(Error::config(format!(
            "The active account changed while {} was being verified (now {}). Nothing was changed. Re-run when no other login is in flight.",
            verified.email,
            now_email.as_deref().unwrap_or("unknown")
        )))
    }

    fn read_live_config_text(&self) -> Result<String> {
        std::fs::read_to_string(self.paths.global_config_path()).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::config("Claude config file not found"),
            std::io::ErrorKind::PermissionDenied => {
                Error::config("Permission denied reading Claude config")
            }
            _ => Error::config(format!("Could not read Claude config: {e}")),
        })
    }

    fn commit_locks(&self) -> Result<CommitLocks> {
        let store = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let credentials = locks::claude_credentials_lock(self.paths, None)?;
        let config = locks::claude_config_lock(self.paths, None)?;
        Ok(CommitLocks {
            _config: config,
            _credentials: credentials,
            _store: store,
        })
    }

    fn roster(&self) -> Result<JsonObject> {
        store::read_sequence(self.paths)?
            .ok_or_else(|| Error::config("No accounts are managed yet"))
    }

    fn drop_record(&self, num: &str) -> Result<()> {
        let mut data = self.roster()?;
        let num_int: i64 = num.parse().unwrap_or_default();
        let sequence = sequence_mut(&mut data);
        if let Some(pos) = sequence.iter().position(|v| v.as_i64() == Some(num_int)) {
            sequence.remove(pos);
        }
        accounts_mut(&mut data).shift_remove(num);
        store::write_sequence(self.paths, &data)
    }

    fn delete_account_files(&self, num: &str, email: &str) {
        self.secrets.delete_backup(num, email);
        store::delete_account_config(self.paths, num, email);
    }

    fn prune_mappings(&self, email: &str, org: &str, console: &mut dyn Console) {
        if let Ok(pruned) = MappingStore::new(&self.paths.backup_root()).prune_account(email, org)
            && pruned > 0
        {
            console.line(&self.style.dimmed(&format!(
                "Removed {pruned} directory mapping(s) for this account"
            )));
        }
    }

    fn clear_dead_token(&self, num: &str, email: &str, org: &str) -> Result<()> {
        UsageStore::new(&self.paths.usage_cache_dir())
            .clear_dead_token(&[(num.to_string(), (email.to_string(), org.to_string()))])
    }

    /// Inside a `run` session profile the "live store" is the profile, not
    /// the default login.
    pub(crate) fn refuse_session_shell(&self) -> Result<()> {
        let Some(dir) = self.env.non_empty("CLAUDE_CONFIG_DIR") else {
            return Ok(());
        };
        if resolve_lenient(Path::new(&dir)).starts_with(resolve_lenient(&self.paths.sessions_dir()))
        {
            return Err(Error::new(
                ErrorKind::SwitchError,
                "This shell is inside a cswap run session profile (CLAUDE_CONFIG_DIR points at it). Mutating accounts here would operate on the wrong live store — unset CLAUDE_CONFIG_DIR or run from a normal shell.",
            ));
        }
        Ok(())
    }
}

fn reject_alias_in_use(data: &JsonObject, alias: &str, own: &str) -> Result<()> {
    match store::find_account_by_alias(data, alias) {
        Some(other) if other != own => Err(Error::new(
            ErrorKind::ValidationError,
            format!("Alias '{alias}' is already used by account {other}"),
        )),
        _ => Ok(()),
    }
}

fn record_text(data: &JsonObject, num: &str, key: &str) -> String {
    store::accounts(data)
        .and_then(|a| a.get(num))
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn accounts_mut(data: &mut JsonObject) -> &mut JsonObject {
    if !data.get("accounts").is_some_and(Value::is_object) {
        data.insert("accounts".into(), json!({}));
    }
    data.get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("accounts is an object")
}

fn sequence_mut(data: &mut JsonObject) -> &mut Vec<Value> {
    if !data.get("sequence").is_some_and(Value::is_array) {
        data.insert("sequence".into(), json!([]));
    }
    data.get_mut("sequence")
        .and_then(Value::as_array_mut)
        .expect("sequence is an array")
}

pub fn display_tag(org_name: &str) -> &str {
    if org_name.is_empty() {
        "personal"
    } else {
        org_name
    }
}

/// `Path.resolve(strict=False)`: symlinks resolved as far as the path exists.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve_lenient(parent).join(name),
        _ => path.to_path_buf(),
    }
}
