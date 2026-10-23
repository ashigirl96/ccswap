use std::path::PathBuf;

use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::identifier::{self, Picker};
use ccswap_core::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use ccswap_core::pyjson::py_str;
use serde_json::Value;

use super::{Accounts, Console, accounts_mut, sequence_mut};
use crate::migrations::LEGACY_KEYRING_SERVICE;
use crate::secret_store::{BACKUP_SERVICE, SecretStore, profile_service_name};
use crate::session_profile::{self, join_pids, scan_live_sessions};
use crate::store::{self, JsonObject};

/// The session profiles a purge removes, checked to have no live Claude.
#[derive(Debug)]
pub struct ClaudePurge {
    session_dirs: Vec<PathBuf>,
}

impl ClaudePurge {
    pub fn has_session_profiles(&self) -> bool {
        !self.session_dirs.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccount {
    pub number: String,
    pub email: String,
    pub organization_uuid: String,
}

impl Accounts<'_> {
    /// Deletes a slot's backups and record after confirmation (skipped with
    /// `assume_yes`). `activeAccountNumber` is left as it was.
    pub fn remove_account(
        &self,
        identifier: &str,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        self.refuse_session_shell()?;
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        store::read_sequence_migrated(self.paths)?;
        let style = self.style;

        let candidates = store::read_sequence(self.paths)?
            .map(|data| store::identifier_candidates(&data))
            .unwrap_or_default();
        let picker = Picker {
            console: &mut *console,
            style,
            action: "remove",
        };
        let Some(identifier) = identifier::screen(&candidates, identifier, Some(picker))? else {
            return Ok(());
        };

        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        let num = resolve_existing(&data, &identifier)?;
        let email = record(&data, &num)
            .and_then(|r| r.get("email"))
            .map_or("None".into(), py_str);
        let org = text_field(&data, &num, "organizationUuid");
        session_profile::ensure_no_live_session(self.paths, &num, &email, "--remove-account")?;

        if active_number(&data) == num {
            console.line(&style.yellowed(&format!(
                "Warning: Account-{num} ({email}) is currently active"
            )));
        }
        if !assume_yes {
            let question = format!(
                "Are you sure you want to permanently remove Account-{num} ({email})? [y/N] "
            );
            match console.prompt(&question) {
                Some(answer) if answer.to_lowercase() == "y" => {}
                Some(_) => {
                    console.line(&style.dimmed("Cancelled"));
                    return Ok(());
                }
                None => {
                    console.line(&format!("\n{}", style.dimmed("Cancelled")));
                    return Ok(());
                }
            }
        }

        self.delete_account_files(&num, &email)?;
        {
            let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
            let mut data = self.roster()?;
            accounts_mut(&mut data).shift_remove(&num);
            let num_int = num.parse::<i64>().ok();
            sequence_mut(&mut data).retain(|v| v.as_i64() != num_int);
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, &data)?;
        }
        ccswap_core::log::info(format!("Removed account {num}: {email}"));
        console.line(&format!(
            "{} Account-{num} ({email})",
            style.accent("Removed")
        ));
        self.prune_mappings(&email, &org, console);
        Ok(())
    }

    /// Holds a slot out of automatic rotation (`disabled: true`) or returns
    /// it (the key is removed). It stays an explicit switch target.
    pub fn set_account_disabled(
        &self,
        identifier: &str,
        disabled: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        let ResolvedAccount {
            number: num, email, ..
        } = self.resolve_account(identifier)?;
        let style = self.style;
        let verb = if disabled { "disabled" } else { "enabled" };
        let data = {
            let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
            let mut data = store::read_sequence(self.paths)?.unwrap_or_default();
            let Some(record) = accounts_mut(&mut data)
                .get_mut(&num)
                .and_then(Value::as_object_mut)
                .filter(|r| !r.is_empty())
            else {
                return Err(Error::new(
                    ErrorKind::AccountNotFoundError,
                    format!("Account-{num} does not exist"),
                ));
            };
            if store::is_truthy(record.get("disabled")) == disabled {
                console.line(&style.dimmed(&format!("Account-{num} ({email}) is already {verb}.")));
                return Ok(());
            }
            if disabled {
                record.insert("disabled".into(), true.into());
            } else {
                record.shift_remove("disabled");
            }
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, &data)?;
            data
        };

        let capitalized = if disabled { "Disabled" } else { "Enabled" };
        ccswap_core::log::info(format!("{capitalized} account {num}: {email}"));
        console.line(&format!(
            "{} Account-{num} ({email}).",
            style.accent(capitalized)
        ));
        if !disabled {
            console.line(&style.dimmed("  It is back in the rotation."));
            return Ok(());
        }
        if active_number(&data) == num {
            console.line(&style.dimmed(
                "  It is the active account — it stays live until you switch away; it just won't be an automatic switch target.",
            ));
        }
        if self.switchable_account_numbers()?.is_empty() {
            console.line(&style.yellowed(
                "  No accounts remain in rotation — auto-switch and bare switch have nothing to pick. Re-enable one with ccswap enable <num|email>.",
            ));
        }
        Ok(())
    }

    /// Slots eligible for automatic selection, in `sequence` order: a stored
    /// credential and config exist and the slot is not disabled.
    pub fn switchable_account_numbers(&self) -> Result<Vec<String>> {
        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        let sequence = data
            .get("sequence")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(sequence
            .iter()
            .map(py_str)
            .filter(|num| {
                let Some(rec) = record(&data, num).filter(|r| !r.is_empty()) else {
                    return false;
                };
                if store::is_truthy(rec.get("disabled")) {
                    return false;
                }
                let email = rec.get("email").and_then(Value::as_str).unwrap_or_default();
                !self.secrets.read_backup(num, email).value.is_empty()
                    && !store::read_account_config(self.paths, num, email).is_empty()
            })
            .collect())
    }

    /// Sets (or renames) a slot's alias; returns `(slot, normalized alias)`.
    pub fn set_alias(&self, identifier: &str, alias: &str) -> Result<(String, String)> {
        self.refuse_session_shell()?;
        let normalized = identifier::normalize_alias(alias)?;
        let num = self.resolve_account(identifier)?.number;
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let mut data = store::read_sequence(self.paths)?.unwrap_or_default();
        resolve_existing(&data, &num)?;
        if let Some(other) = store::find_account_by_alias(&data, &normalized)
            && other != num
        {
            return Err(Error::config(format!(
                "Alias '{normalized}' is already used by account {other}"
            )));
        }
        if let Some(rec) = accounts_mut(&mut data)
            .get_mut(&num)
            .and_then(Value::as_object_mut)
        {
            rec.insert("alias".into(), normalized.clone().into());
        }
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.paths, &data)?;
        Ok((num, normalized))
    }

    /// Clears a slot's alias (a no-op when it has none); returns the slot.
    pub fn unset_alias(&self, identifier: &str) -> Result<String> {
        self.refuse_session_shell()?;
        let num = self.resolve_account(identifier)?.number;
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let mut data = store::read_sequence(self.paths)?.unwrap_or_default();
        resolve_existing(&data, &num)?;
        let removed = accounts_mut(&mut data)
            .get_mut(&num)
            .and_then(Value::as_object_mut)
            .and_then(|rec| rec.shift_remove("alias"))
            .is_some();
        if removed {
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, &data)?;
        }
        Ok(num)
    }

    /// Every set alias as `(slot, alias, email)`, by slot number.
    pub fn list_aliases(&self) -> Result<Vec<(String, String, String)>> {
        store::read_sequence_migrated(self.paths)?;
        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        let mut rows: Vec<(String, String, String)> = store::accounts(&data)
            .map(|accounts| {
                accounts
                    .iter()
                    .filter(|(_, rec)| store::is_truthy(rec.get("alias")))
                    .map(|(num, rec)| {
                        (
                            num.clone(),
                            rec.get("alias").map_or("None".into(), py_str),
                            rec.get("email")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by_key(|(num, _, _)| num.parse::<i64>().unwrap_or(i64::MAX));
        Ok(rows)
    }

    /// Claude Code's share of `ccswap purge`, refused while a session-mode
    /// Claude could still be using a profile it would remove.
    pub fn prepare_purge(&self) -> Result<ClaudePurge> {
        self.refuse_session_shell()?;
        let session_dirs: Vec<PathBuf> = std::fs::read_dir(self.paths.sessions_dir())
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        refuse_live_profiles(&session_dirs)?;
        Ok(ClaudePurge { session_dirs })
    }

    /// Deletes every stored credential (both backends, plus the legacy
    /// `account-None-*` keys) and the session profiles' Keychain entries, and
    /// returns what went. The store directory is left to the caller; the live
    /// login is untouched.
    pub fn purge_credentials(&self, purge: &ClaudePurge) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        let keychain = self.secrets.keychain();
        if let Some(data) = store::read_sequence(self.paths)? {
            for (num, rec) in store::accounts(&data).into_iter().flatten() {
                let email = rec.get("email").and_then(Value::as_str).unwrap_or_default();
                let mut nums = vec![num.as_str()];
                if num != "None" {
                    nums.push("None");
                }
                for n in &nums {
                    let enc = self.secrets.backup_enc_path(n, email);
                    if enc.exists() && std::fs::remove_file(&enc).is_ok() {
                        let name = enc.file_name().unwrap_or_default().to_string_lossy();
                        removed.push(format!("Credential file: {name}"));
                    }
                }
                let usernames: Vec<String> = nums
                    .iter()
                    .map(|n| SecretStore::backup_account_name(n, email))
                    .collect();
                for username in &usernames {
                    if keychain.delete_password(BACKUP_SERVICE, username).is_ok() {
                        removed.push(format!("Credential: {username}"));
                    }
                }
                for username in &usernames {
                    if keychain
                        .delete_if_present(LEGACY_KEYRING_SERVICE, username)
                        .unwrap_or(false)
                    {
                        removed.push(format!("Legacy keyring credential: {username}"));
                    }
                }
            }
        }

        if purge.has_session_profiles() {
            for dir in &purge.session_dirs {
                let service = profile_service_name(&dir.to_string_lossy());
                let _ = keychain.delete_password(&service, keychain.account());
            }
            let names: Vec<String> = purge
                .session_dirs
                .iter()
                .map(|d| {
                    d.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            removed.push(format!("Session profiles: {}", names.join(", ")));
        }
        Ok(removed)
    }

    /// An ambiguous email is an error, never a prompt.
    pub fn resolve_account(&self, identifier: &str) -> Result<ResolvedAccount> {
        store::read_sequence_migrated(self.paths)?;
        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        let number = resolve_existing(&data, identifier)?;
        Ok(ResolvedAccount {
            email: text_field(&data, &number, "email"),
            organization_uuid: text_field(&data, &number, "organizationUuid"),
            number,
        })
    }
}

/// Purging would pull a profile out from under a live Claude, and an
/// unreadable record cannot rule one out.
fn refuse_live_profiles(session_dirs: &[PathBuf]) -> Result<()> {
    let mut live = Vec::new();
    let mut unreadable = Vec::new();
    for dir in session_dirs {
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        let (sessions, bad) = scan_live_sessions(dir);
        if !sessions.is_empty() {
            let pids: Vec<i32> = sessions.iter().map(|s| s.pid).collect();
            live.push(format!("{name} (PID {})", join_pids(&pids)));
        } else if bad > 0 {
            unreadable.push(format!("{name} ({bad} record(s))"));
        }
    }
    if !live.is_empty() {
        return Err(Error::new(
            ErrorKind::SessionError,
            format!(
                "Live session-mode Claude instance(s) found: {}. Exit them first, then retry --purge.",
                live.join("; ")
            ),
        ));
    }
    if !unreadable.is_empty() {
        return Err(Error::new(
            ErrorKind::SessionError,
            format!(
                "Session records that could not be read: {}. Whether a Claude instance is live cannot be determined, and purging would pull a live profile out from under it. Repair or remove them, then retry --purge.",
                unreadable.join("; ")
            ),
        ));
    }
    Ok(())
}

/// Resolves `identifier` to a slot whose record exists (and is non-empty).
fn resolve_existing(data: &JsonObject, identifier: &str) -> Result<String> {
    let present = (!data.is_empty()).then_some(data);
    let num = store::resolve_account_identifier(present, identifier)?.ok_or_else(|| {
        Error::new(
            ErrorKind::AccountNotFoundError,
            format!("No account found with identifier: {identifier}"),
        )
    })?;
    if !store::is_truthy(store::accounts(data).and_then(|a| a.get(&num))) {
        return Err(Error::new(
            ErrorKind::AccountNotFoundError,
            format!("Account-{num} does not exist"),
        ));
    }
    Ok(num)
}

fn record<'a>(data: &'a JsonObject, num: &str) -> Option<&'a JsonObject> {
    store::accounts(data)?.get(num)?.as_object()
}

/// A string field, `""` when absent or null.
fn text_field(data: &JsonObject, num: &str, key: &str) -> String {
    record(data, num)
        .and_then(|r| r.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `str(activeAccountNumber)`.
fn active_number(data: &JsonObject) -> String {
    data.get("activeAccountNumber")
        .map_or("None".into(), py_str)
}
