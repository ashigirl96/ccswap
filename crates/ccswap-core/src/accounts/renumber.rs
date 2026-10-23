//! Directory mappings and usage-cache rows key on identity, so a renumber
//! leaves them alone.
//!
//! The `sequence.json` write is the commit point: destination keys are set
//! before it and old keys cleared only after it, so the roster never points
//! at missing material.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{Accounts, Console, accounts_mut, sequence_mut};
use crate::error::{Error, ErrorKind, Result};
use crate::identifier;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::session_profile;
use crate::store::{self, JsonObject};

/// `cap` stays at least this; `add` numbers from the highest slot, so a
/// stray huge target would inflate every future account number.
const MIN_SLOT_CAP: u64 = 99;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    pub source: String,
    pub target: String,
    /// The target was occupied and its account took the source slot.
    pub swapped: bool,
}

/// One account's slot and the material read before anything moved.
struct Side {
    num: String,
    email: String,
    creds: String,
    config: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Creds,
    Config,
}

impl Accounts<'_> {
    /// Exchanges two accounts' slot numbers; returns the resolved numbers.
    pub fn swap_accounts(
        &self,
        first: &str,
        second: &str,
        console: &mut dyn Console,
    ) -> Result<(String, String)> {
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        self.refuse_session_shell()?;
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        Renumber { acc: self, console }.swap(first, second)
    }

    /// Puts `account` in slot `target`: relocates into an empty slot, trades
    /// places with an occupant, or does nothing when it is already there.
    pub fn move_account(
        &self,
        account: &str,
        target: &str,
        console: &mut dyn Console,
    ) -> Result<MoveOutcome> {
        self.refuse_session_shell()?;
        if !self.paths.sequence_file().exists() {
            return Err(Error::config("No accounts are managed yet"));
        }
        let target = normalize_target(target)?;
        // Resolution happens under the same lock as the mutation: a number
        // resolved outside it could be renumbered by a concurrent swap.
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let data = store::read_sequence_migrated(self.paths)?;
        let source = store::resolve_account_identifier(data.as_ref(), account)?
            .ok_or_else(|| not_found(format!("No account found with identifier: {account}")))?;
        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        if record(&data, &source).is_none() {
            return Err(not_found(format!("Account-{source} does not exist")));
        }
        let max_slot = store::accounts(&data)
            .map(|a| {
                a.keys()
                    .filter(|k| identifier::is_slot_number(k))
                    .filter_map(|k| k.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let cap = MIN_SLOT_CAP.max(max_slot);
        if target.parse::<u64>().map_or(true, |t| t > cap) {
            return Err(validation(format!(
                "Target slot {target} is out of range (1-{cap}): new accounts are numbered from the highest slot, so a large target would inflate future account numbers"
            )));
        }
        let mut outcome = MoveOutcome {
            source: source.clone(),
            target: target.clone(),
            swapped: false,
        };
        if source == target {
            return Ok(outcome);
        }
        let mut renumber = Renumber { acc: self, console };
        if record(&data, &target).is_some() {
            renumber.swap(&source, &target)?;
            outcome.swapped = true;
        } else {
            renumber.relocate(&source, &target)?;
        }
        Ok(outcome)
    }
}

struct Renumber<'r, 'a> {
    acc: &'r Accounts<'a>,
    console: &'r mut dyn Console,
}

impl Renumber<'_, '_> {
    fn swap(&mut self, first: &str, second: &str) -> Result<(String, String)> {
        let paths = self.acc.paths;
        let data = store::read_sequence_migrated(paths)?;
        let num_a = store::resolve_account_identifier(data.as_ref(), first)?
            .ok_or_else(|| not_found(format!("No account found with identifier: {first}")))?;
        let num_b = store::resolve_account_identifier(data.as_ref(), second)?
            .ok_or_else(|| not_found(format!("No account found with identifier: {second}")))?;
        if num_a == num_b {
            return Err(validation("Cannot swap an account with itself".into()));
        }
        let mut data = store::read_sequence(paths)?.unwrap_or_default();
        let record_a = record(&data, &num_a)
            .cloned()
            .ok_or_else(|| not_found(format!("Account-{num_a} does not exist")))?;
        let record_b = record(&data, &num_b)
            .cloned()
            .ok_or_else(|| not_found(format!("Account-{num_b} does not exist")))?;
        let (email_a, email_b) = (email_of(&record_a), email_of(&record_b));
        session_profile::ensure_no_live_session(paths, &num_a, &email_a, "--swap-accounts")?;
        session_profile::ensure_no_live_session(paths, &num_b, &email_b, "--swap-accounts")?;
        let a = self.snapshot(num_a, email_a)?;
        let b = self.snapshot(num_b, email_b)?;

        let mut staging = Vec::new();
        let result = self.swap_before_commit(&a, &b, record_a, record_b, &mut data, &mut staging);
        if let Err(error) = result {
            self.rollback_swap(&a, &b, &staging);
            return Err(error);
        }

        if a.email != b.email {
            for side in [&a, &b] {
                self.delete_old_key(&side.num, &side.email);
            }
        }
        // The .prev generations written while overwriting the destination
        // keys hold displaced material that recovery must never resurrect.
        if !a.creds.is_empty() {
            self.acc.secrets.delete_previous_backup(&b.num, &a.email);
        }
        if !b.creds.is_empty() {
            self.acc.secrets.delete_previous_backup(&a.num, &b.email);
        }
        self.discard_staging(&staging);
        crate::log::info(format!(
            "Swapped slots: {} ({}) <-> {} ({})",
            a.num, a.email, b.num, b.email
        ));
        Ok((a.num, b.num))
    }

    fn swap_before_commit(
        &mut self,
        a: &Side,
        b: &Side,
        record_a: Value,
        record_b: Value,
        data: &mut JsonObject,
        staging: &mut Vec<PathBuf>,
    ) -> Result<()> {
        if a.email == b.email {
            // Overlapping keys: each write destroys the other account's
            // material, so park durable copies first.
            *staging = self.stage_overlap_material(&[a, b])?;
        }
        self.swap_session_dirs(&a.num, &a.email, &b.num, &b.email);
        self.set_key(&b.num, &a.email, a)?;
        self.set_key(&a.num, &b.email, b)?;

        let accounts = accounts_mut(data);
        accounts.insert(a.num.clone(), record_b);
        accounts.insert(b.num.clone(), record_a);
        let (int_a, int_b) = (slot_int(&a.num), slot_int(&b.num));
        renumber_sequence(data, |n| {
            if n == int_a {
                int_b
            } else if n == int_b {
                int_a
            } else {
                n
            }
        });
        match data.get("activeAccountNumber").and_then(Value::as_i64) {
            Some(active) if active == int_a => {
                data.insert("activeAccountNumber".into(), int_b.into());
            }
            Some(active) if active == int_b => {
                data.insert("activeAccountNumber".into(), int_a.into());
            }
            _ => {}
        }
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.acc.paths, data)
    }

    fn relocate(&mut self, source: &str, target: &str) -> Result<()> {
        let paths = self.acc.paths;
        let mut data = store::read_sequence(paths)?.unwrap_or_default();
        let Some(record_value) = record(&data, source).cloned() else {
            return Err(not_found(format!("Account-{source} does not exist")));
        };
        if record(&data, target).is_some() {
            return Err(validation(format!(
                "Slot {target} is already occupied — retry the move"
            )));
        }
        let email = email_of(&record_value);
        session_profile::ensure_no_live_session(paths, source, &email, "--move-account")?;
        let side = self.snapshot(source.to_string(), email)?;
        let email = &side.email;
        let src_dir = session_profile::session_dir_for(paths, source, email);
        let dst_dir = session_profile::session_dir_for(paths, target, email);

        let result = (|| -> Result<()> {
            if src_dir.exists()
                && !dst_dir.exists()
                && let Err(e) = std::fs::rename(&src_dir, &dst_dir)
            {
                crate::log::warning(format!("Session profile move skipped during move: {e}"));
            }
            self.set_key(target, email, &side)?;
            let accounts = accounts_mut(&mut data);
            accounts.insert(target.to_string(), record_value);
            accounts.shift_remove(source);
            let (int_src, int_target) = (slot_int(source), slot_int(target));
            renumber_sequence(&mut data, |n| if n == int_src { int_target } else { n });
            if data.get("activeAccountNumber").and_then(Value::as_i64) == Some(int_src) {
                data.insert("activeAccountNumber".into(), int_target.into());
            }
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(paths, &data)
        })();
        if let Err(error) = result {
            self.acc.secrets.delete_backup(target, email);
            let cleanup = delete_config_strict(paths, target, email).and_then(|()| {
                if dst_dir.exists() && !src_dir.exists() {
                    std::fs::rename(&dst_dir, &src_dir)
                        .map_err(|e| Error::config(e.to_string()))?;
                }
                Ok(())
            });
            if let Err(e) = cleanup {
                crate::log::error(format!("Cleanup after failed move incomplete: {e}"));
            }
            return Err(error);
        }

        self.delete_old_key(source, email);
        if !side.creds.is_empty() {
            self.acc.secrets.delete_previous_backup(target, email);
        }
        crate::log::info(format!("Moved slot: {source} ({email}) -> {target}"));
        Ok(())
    }

    /// An unreadable (not absent) credential aborts: carrying it as "" would
    /// drop the slot's live refresh token at its new number.
    fn snapshot(&self, num: String, email: String) -> Result<Side> {
        let read = self.acc.secrets.read_backup(&num, &email);
        if read.unreadable {
            return Err(Error::config(format!(
                "Account-{num}'s stored credential could not be read (keychain unavailable?); nothing was changed. Retry once it is readable again."
            )));
        }
        let config = store::read_account_config(self.acc.paths, &num, &email);
        Ok(Side {
            num,
            email,
            creds: read.value,
            config,
        })
    }

    /// Clears what `owner` lacks, so an unbacked account never adopts leftover
    /// material under its new key.
    fn set_key(&self, num: &str, email: &str, owner: &Side) -> Result<()> {
        self.restore(Kind::Creds, num, email, &owner.creds, true)?;
        self.restore(Kind::Config, num, email, &owner.config, true)
    }

    fn restore(&self, kind: Kind, num: &str, email: &str, value: &str, clear: bool) -> Result<()> {
        let paths = self.acc.paths;
        match (kind, value.is_empty()) {
            (Kind::Creds, false) => self.write_credentials(num, email, value),
            (Kind::Config, false) => store::write_account_config(paths, num, email, value),
            (_, true) if !clear => Ok(()),
            (Kind::Creds, true) => self.acc.secrets.delete_backup_strict(num, email),
            (Kind::Config, true) => delete_config_strict(paths, num, email),
        }
    }

    /// A backup write supersedes whatever the slot's session profile was
    /// seeded with.
    fn write_credentials(&self, num: &str, email: &str, creds: &str) -> Result<()> {
        self.acc.secrets.write_backup(num, email, creds)?;
        session_profile::after_backup_write(
            self.acc.secrets.keychain(),
            self.acc.paths,
            num,
            email,
        );
        Ok(())
    }

    /// Best-effort removal of a vacated key; a leftover is unreferenced but
    /// would poison a later same-email account landing on that number.
    fn delete_old_key(&self, num: &str, email: &str) {
        self.acc.secrets.delete_backup(num, email);
        if let Err(e) = delete_config_strict(self.acc.paths, num, email) {
            crate::log::error(format!(
                "Stale backup left under old key {num} ({email}): {e}"
            ));
        }
        session_profile::delete_slot_profile(
            self.acc.secrets.keychain(),
            self.acc.paths,
            num,
            email,
        );
    }

    /// Same-email paths swap directly, hence the temporary name; a profile
    /// that cannot move costs only its history, as `run` re-seeds it.
    fn swap_session_dirs(&self, num_a: &str, email_a: &str, num_b: &str, email_b: &str) {
        let dir =
            |num: &str, email: &str| session_profile::session_dir_for(self.acc.paths, num, email);
        let (dir_a, dir_b) = (dir(num_a, email_a), dir(num_b, email_b));
        let (new_a, new_b) = (dir(num_b, email_a), dir(num_a, email_b));
        let mut parked: Option<PathBuf> = None;
        let moved = (|| -> std::io::Result<()> {
            if dir_a.exists() {
                let name = dir_a.file_name().unwrap_or_default().to_string_lossy();
                let temp = dir_a.with_file_name(format!("{name}.swapping"));
                std::fs::rename(&dir_a, &temp)?;
                parked = Some(temp);
            }
            if dir_b.exists() && !new_b.exists() {
                std::fs::rename(&dir_b, &new_b)?;
            }
            if let Some(temp) = &parked
                && !new_a.exists()
            {
                std::fs::rename(temp, &new_a)?;
                parked = None;
            }
            Ok(())
        })();
        if let Err(e) = moved {
            crate::log::warning(format!("Session profile move skipped during swap: {e}"));
        }
        if let Some(temp) = parked
            && !dir_a.exists()
        {
            let _ = std::fs::rename(temp, &dir_a);
        }
    }

    /// A leftover from an interrupted swap may be the only surviving copy of a
    /// credential, so it is never overwritten.
    fn stage_overlap_material(&mut self, sides: &[&Side]) -> Result<Vec<PathBuf>> {
        let dir = self.acc.paths.credentials_dir();
        let mut staged = Vec::new();
        for side in sides {
            for (kind, content) in [("creds", &side.creds), ("config", &side.config)] {
                if content.is_empty() {
                    continue;
                }
                let path = dir.join(format!(".swap-staging-{kind}-{}.json", side.num));
                if path.exists() {
                    self.discard_staging(&staged);
                    return Err(Error::config(format!(
                        "Found leftover staging from an interrupted swap: {}. It holds that slot's pre-swap credentials and may be the only surviving copy. Verify both accounts still work (`ccswap list`), then delete the file and retry.",
                        path.display()
                    )));
                }
                if let Err(e) = write_exclusive(&path, content) {
                    self.discard_staging(&staged);
                    return Err(Error::config(format!(
                        "Could not stage swap material, nothing was changed: {e}"
                    )));
                }
                staged.push(path);
            }
        }
        Ok(staged)
    }

    fn discard_staging(&mut self, staging: &[PathBuf]) {
        for path in staging {
            if std::fs::remove_file(path).is_err() {
                let style = self.acc.style;
                self.console.line(&style.yellowed(&format!(
                    "Could not remove swap staging file {} — it holds pre-swap credentials; please delete it manually.",
                    path.display()
                )));
            }
        }
    }

    /// Every step is attempted; if any fails, the staged copies stay on disk
    /// for manual recovery.
    fn rollback_swap(&mut self, a: &Side, b: &Side, staging: &[PathBuf]) {
        crate::log::error(format!(
            "Swap {} <-> {} failed mid-write; restoring both slots",
            a.num, b.num
        ));
        self.swap_session_dirs(&b.num, &a.email, &a.num, &b.email);
        let overlap = a.email == b.email;
        let mut failures = 0;
        for (kind, side) in [
            (Kind::Creds, a),
            (Kind::Config, a),
            (Kind::Creds, b),
            (Kind::Config, b),
        ] {
            let original = match kind {
                Kind::Creds => &side.creds,
                Kind::Config => &side.config,
            };
            // An originally empty overlapping key may now hold the other
            // account's material and must read empty again.
            if let Err(e) = self.restore(kind, &side.num, &side.email, original, overlap) {
                failures += 1;
                let kind = match kind {
                    Kind::Creds => "creds",
                    Kind::Config => "config",
                };
                crate::log::error(format!(
                    "Rollback {kind} restore failed for slot {}: {e}",
                    side.num
                ));
            }
        }
        if !overlap {
            for (num, email) in [(&b.num, &a.email), (&a.num, &b.email)] {
                self.acc.secrets.delete_backup(num, email);
                if let Err(e) = delete_config_strict(self.acc.paths, num, email) {
                    failures += 1;
                    crate::log::error(format!("Rollback cleanup failed for slot {num}: {e}"));
                }
            }
        }
        if failures == 0 {
            for side in [a, b] {
                if !side.creds.is_empty() {
                    self.acc
                        .secrets
                        .delete_previous_backup(&side.num, &side.email);
                }
            }
        }
        if staging.is_empty() {
            return;
        }
        if failures > 0 {
            let kept: Vec<String> = staging.iter().map(|p| p.display().to_string()).collect();
            crate::log::error(format!(
                "Rollback incomplete — staged pre-swap copies kept for manual recovery: {}",
                kept.join(", ")
            ));
            let style = self.acc.style;
            self.console.line(&style.yellowed(&format!(
                "Swap rollback was incomplete; your pre-swap credentials are preserved in: {}",
                kept.join(", ")
            )));
        } else {
            self.discard_staging(staging);
        }
    }
}

/// Positive slot number with leading zeros dropped ("05" -> "5").
fn normalize_target(raw: &str) -> Result<String> {
    let target = raw.trim();
    let digits = target.trim_start_matches('0');
    if !identifier::is_slot_number(target) || digits.is_empty() {
        return Err(validation(format!(
            "Target slot must be a positive slot number, got: {} (use `swap` to trade two accounts by identifier)",
            py_repr(target)
        )));
    }
    Ok(digits.to_string())
}

/// A record counts only when it is a non-empty object (Python truthiness).
fn record<'d>(data: &'d JsonObject, num: &str) -> Option<&'d Value> {
    store::accounts(data)?
        .get(num)
        .filter(|r| r.as_object().is_some_and(|o| !o.is_empty()))
}

fn email_of(record: &Value) -> String {
    record
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn slot_int(num: &str) -> i64 {
    num.parse().unwrap_or_default()
}

/// Renumbers, then sorts: rotation and list order follow the new numbers.
fn renumber_sequence(data: &mut JsonObject, map: impl Fn(i64) -> i64) {
    let sequence = sequence_mut(data);
    for value in sequence.iter_mut() {
        if let Some(n) = value.as_i64() {
            *value = map(n).into();
        }
    }
    sequence.sort_by_key(|v| v.as_i64().unwrap_or_default());
}

/// Unconditional unlink: an existence check reads "absent" on an
/// inaccessible directory and would fail open.
fn delete_config_strict(paths: &crate::paths::Paths, num: &str, email: &str) -> Result<()> {
    let path = store::config_backup_path(paths, num, email);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::config(format!(
            "Could not remove {}: {e}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn write_exclusive(path: &Path, content: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}

fn not_found(message: String) -> Error {
    Error::new(ErrorKind::AccountNotFoundError, message)
}

fn validation(message: String) -> Error {
    Error::new(ErrorKind::ValidationError, message)
}

/// Python's `repr()` of a str, for messages that echo `{value!r}`.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_is_normalized_or_rejected_like_upstream() {
        assert_eq!(normalize_target(" 05 ").unwrap(), "5");
        for bad in ["abc", "0", "00", "-1", "1.5", ""] {
            assert_eq!(
                normalize_target(bad).unwrap_err().kind,
                ErrorKind::ValidationError
            );
        }
        assert!(
            normalize_target("")
                .unwrap_err()
                .message
                .contains("got: '' (use `swap`")
        );
    }

    #[test]
    fn repr_picks_quotes_like_python() {
        assert_eq!(py_repr("abc"), "'abc'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a'\"b"), "'a\\'\"b'");
        assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    }
}
