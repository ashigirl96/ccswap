//! Refresh tokens are one-time-use, so every POST of a backup's goes through upstream's `consume_backup_grant` protocol.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::anthropic_client::{
    AnthropicClient, REFRESH_TIMEOUT, RefreshOutcome, credential_fingerprint, extract_oauth_data,
    is_oauth_token_expired,
};
use crate::env::Environment;
use crate::error::{ErrorKind, Result};
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::log;
use crate::paths::Paths;
use crate::secret_store::SecretStore;
use crate::session_profile;
use crate::store::{self, JsonObject};
use crate::unclaimed::UnclaimedStash;
use crate::usage::truthy;

/// Stash reasons after which the slot still holds the spent generation, so
/// success would be a lie. A removed slot and a CAS conflict (a racing
/// writer's newer lineage was adopted) are deliberately not demoting.
const DEMOTING_STASH_REASONS: [&str; 4] = [
    "consume-gate-persist-failed",
    "consume-gate-persist-lock-failed",
    "consume-gate-unpersisted",
    "consume-gate-store-unreadable",
];

pub fn consume_lock_path(paths: &Paths, num: &str) -> PathBuf {
    paths.credentials_dir().join(format!(".consume-{num}.lock"))
}

/// A backup write as every slot write is done: the store, then the session
/// profile seeded from the old generation.
pub fn write_account_credentials(
    paths: &Paths,
    secrets: &SecretStore,
    num: &str,
    email: &str,
    credentials: &str,
) -> Result<()> {
    secrets.write_backup(num, email, credentials)?;
    session_profile::after_backup_write(secrets.keychain(), paths, num, email);
    Ok(())
}

pub struct ConsumeGate<'a> {
    pub paths: &'a Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
}

/// What the locked pre-POST window decided.
enum Prepared {
    Done(RefreshOutcome),
    Post {
        input: String,
        consumed_fp: Option<String>,
    },
}

impl ConsumeGate<'_> {
    /// `credentials` on the outcome is the slot's now-current credential,
    /// possibly a racing writer's newer lineage. The caller must not hold the
    /// store lock.
    pub fn consume_backup_grant(&self, num: &str, email: &str, snapshot: &str) -> RefreshOutcome {
        if self
            .env
            .non_empty("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .is_some()
        {
            log::warning(format!(
                "CLAUDE_SECURESTORAGE_CONFIG_DIR is set; cswap mirrors it when capturing a credential but not when consuming one, so refusing to consume account {num}'s refresh token (unset the variable or run from a normal shell)."
            ));
            return RefreshOutcome::failed("store-unmirrored");
        }
        let lock = match StoreLock::try_acquire(
            &consume_lock_path(self.paths, num),
            STORE_LOCK_TIMEOUT,
        ) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                log::info(format!(
                    "Another consume is in flight for account {num}; deferring to the next pass."
                ));
                return RefreshOutcome::failed("consume-busy");
            }
            Err(e) => {
                log::warning(format!(
                    "Pre-consume window failed for account {num}; deferring. ({e})"
                ));
                return RefreshOutcome::failed("transient");
            }
        };
        let outcome = self.consume_locked(num, email, snapshot);
        drop(lock);
        outcome
    }

    fn consume_locked(&self, num: &str, email: &str, snapshot: &str) -> RefreshOutcome {
        let (input, consumed_fp) = match self.prepare(num, email) {
            Ok(Prepared::Done(outcome)) => return outcome,
            Ok(Prepared::Post { input, consumed_fp }) => (input, consumed_fp),
            Err(e) if e.kind == ErrorKind::LockError => {
                log::info(format!(
                    "Slot lock held elsewhere; deferring account {num}'s backup refresh to the next pass."
                ));
                return RefreshOutcome::failed("transient");
            }
            Err(e) => {
                log::warning(format!(
                    "Pre-consume window failed for account {num}; deferring. ({e})"
                ));
                return RefreshOutcome::failed("transient");
            }
        };

        let input_oauth = extract_oauth_data(&input).unwrap_or_default();
        let input_at = input_oauth.get("accessToken").filter(|v| truthy(v));
        let snapshot_at = extract_oauth_data(snapshot)
            .and_then(|o| o.get("accessToken").cloned())
            .filter(truthy);
        if let (Some(input_at), Some(snapshot_at)) = (input_at, snapshot_at.as_ref())
            && input_at != snapshot_at
            && !is_oauth_token_expired(input_oauth.get("expiresAt"))
        {
            // The world already moved past the caller's snapshot and the
            // current generation is fresh: the refresh effectively happened.
            return RefreshOutcome {
                credentials: Some(input),
                consumed_fp,
                ..Default::default()
            };
        }

        let result = self
            .client
            .try_refresh_oauth_credentials(&input, REFRESH_TIMEOUT);
        let Some(successor) = result
            .credentials
            .clone()
            .filter(|c| result.error.is_none() && !c.is_empty())
        else {
            return RefreshOutcome {
                consumed_fp,
                ..result
            };
        };
        self.persist(num, email, successor, result, consumed_fp)
    }

    /// Under the store lock: adopt a stashed successor, re-read, and prefer
    /// a quiescent session profile's newer generation (resyncing the backup
    /// to it). Writes may happen here; the POST never does.
    fn prepare(&self, num: &str, email: &str) -> Result<Prepared> {
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let read = self.secrets.read_backup(num, email);
        if read.unreadable {
            log::info(format!(
                "Backup for account {num} unreadable (keychain); deferring the refresh."
            ));
            return Ok(Prepared::Done(RefreshOutcome::failed("transient")));
        }
        let mut current = read.value;
        match self.adopt_stashed_successor(num, email, &current)? {
            Adopt::Unreadable => {
                log::info(format!(
                    "Account {num}'s stashed successor is unreadable; deferring the refresh."
                ));
                return Ok(Prepared::Done(RefreshOutcome::failed("stash-unreadable")));
            }
            Adopt::Adopted(creds) => current = creds,
            Adopt::Nothing => {}
        }
        if current.is_empty() {
            log::info(format!(
                "Account {num}'s stored credential is gone; deferring the refresh rather than consuming a grant for a slot that no longer exists."
            ));
            return Ok(Prepared::Done(RefreshOutcome::failed("transient")));
        }
        let mut input = current;
        let org_uuid = store::read_sequence(self.paths)?
            .map(|data| store::slot_identity(&data, num).organization_uuid)
            .unwrap_or_default();
        if session_profile::live_session_pids(self.paths, num, email).is_empty()
            && let Some(profile) = self.newer_profile_generation(num, email, &org_uuid, &input)
        {
            write_account_credentials(self.paths, self.secrets, num, email, &profile)?;
            input = profile;
        }
        let consumed_fp = credential_fingerprint(&input);
        Ok(Prepared::Post { input, consumed_fp })
    }

    /// A quiescent, unmarked profile of this very identity holding a newer
    /// full token pair: the backup's refresh token is already consumed.
    fn newer_profile_generation(
        &self,
        num: &str,
        email: &str,
        org_uuid: &str,
        input: &str,
    ) -> Option<String> {
        let dir = session_profile::session_dir_for(self.paths, num, email);
        let profile = session_profile::read_credentials(self.secrets.keychain(), &dir)
            .filter(|p| !p.is_empty())?;
        if session_profile::is_stale(&dir)
            || session_profile::identity_drifted(&dir, email, org_uuid)
        {
            return None;
        }
        let profile_oauth = extract_oauth_data(&profile)?;
        let expiry = |oauth: Option<&serde_json::Map<String, Value>>| {
            oauth
                .and_then(|o| o.get("expiresAt"))
                .filter(|v| truthy(v))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        let newer = profile_oauth.get("accessToken").is_some_and(truthy)
            && profile_oauth.get("refreshToken").is_some_and(truthy)
            && credential_fingerprint(&profile) != credential_fingerprint(input)
            && expiry(Some(&profile_oauth)) > expiry(extract_oauth_data(input).as_ref());
        newer.then_some(profile)
    }

    /// Completes a prior gate's failed persist: a stash row whose
    /// `consumedFp` is the generation the slot still stores holds that
    /// generation's live successor. The caller holds the store lock.
    fn adopt_stashed_successor(&self, num: &str, email: &str, current: &str) -> Result<Adopt> {
        let Some(current_fp) = credential_fingerprint(current) else {
            return Ok(Adopt::Nothing);
        };
        let stash = UnclaimedStash::new(self.paths);
        let rows = match stash.rows() {
            Ok(rows) => rows,
            Err("corrupt") if !stash.entry_files_exist() => JsonObject::new(),
            Err(_) => return Ok(Adopt::Unreadable),
        };
        let mut deferred = false;
        for (id, row) in &rows {
            let field = |key: &str| row.get(key).and_then(Value::as_str);
            if field("configSlot") != Some(num) {
                continue;
            }
            if field("consumedFp") != Some(current_fp.as_str()) {
                if field("reason") == Some("consume-gate-cas-conflict") {
                    retire(&stash, id, num);
                    log::info(format!(
                        "Retired account {num}'s CAS-conflict stash entry: its generation was superseded by the writer that won the race, so no pass can ever adopt it."
                    ));
                } else if stash.read_entry(id) == (String::new(), false) {
                    retire(&stash, id, num);
                    log::info(format!(
                        "Retired account {num}'s byte-less stash entry: its credential is gone and its generation has passed, so no pass could ever adopt it."
                    ));
                }
                continue;
            }
            let (creds, unreadable) = stash.read_entry(id);
            if unreadable {
                deferred = true;
                continue;
            }
            if creds.is_empty() {
                retire(&stash, id, num);
                log::info(format!(
                    "Retired account {num}'s unreadable-bytes stash entry: its generation is gone, so no pass could ever adopt it."
                ));
                continue;
            }
            write_account_credentials(self.paths, self.secrets, num, email, &creds)?;
            retire(&stash, id, num);
            log::info(format!(
                "Adopted account {num}'s stashed successor ({}): the stored generation was already consumed by the gate pass that stashed it.",
                field("reason").unwrap_or("unknown")
            ));
            return Ok(Adopt::Adopted(creds));
        }
        Ok(if deferred {
            Adopt::Unreadable
        } else {
            Adopt::Nothing
        })
    }

    /// After the POST: compare-and-swap on the refresh-token fingerprint,
    /// else park the successor. The grant is spent, so nothing here fails.
    fn persist(
        &self,
        num: &str,
        email: &str,
        successor: String,
        result: RefreshOutcome,
        consumed_fp: Option<String>,
    ) -> RefreshOutcome {
        let mut outcome_creds = successor.clone();
        let mut stashed_reason: Option<&'static str> = None;
        let stash_successor = |reason: &'static str| -> Result<&'static str> {
            let context = json!({
                "reason": reason,
                "configSlot": num,
                "consumedFp": consumed_fp,
                "fingerprint": credential_fingerprint(&successor),
            });
            let Value::Object(context) = context else {
                unreachable!("json! object literal");
            };
            UnclaimedStash::new(self.paths).write(&successor, context)?;
            Ok(reason)
        };
        let attempt = (|| -> Result<()> {
            let lock = match StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT) {
                Ok(lock) => lock,
                Err(e) if e.kind == ErrorKind::LockError => {
                    stashed_reason = Some(stash_successor("consume-gate-persist-lock-failed")?);
                    log::warning(format!(
                        "Slot lock unavailable after consuming account {num}'s grant; successor stashed for the next pass."
                    ));
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let store_now = self.secrets.read_backup(num, email);
            if store_now.unreadable {
                stashed_reason = Some(stash_successor("consume-gate-store-unreadable")?);
                log::warning(format!(
                    "Account {num}'s stored credential was unreadable (keychain) after a refresh POST; successor stashed, nothing rewritten."
                ));
            } else if store_now.value.is_empty() {
                stashed_reason = Some(stash_successor("consume-gate-slot-removed")?);
                log::warning(format!(
                    "Account {num}'s stored credential disappeared during a refresh POST; successor stashed, nothing rewritten."
                ));
            } else if credential_fingerprint(&store_now.value) != consumed_fp {
                stashed_reason = Some(stash_successor("consume-gate-cas-conflict")?);
                log::warning(format!(
                    "Backup lineage for account {num} moved during a refresh POST; successor stashed, adopting the newer store credential."
                ));
                outcome_creds = store_now.value;
            } else {
                write_account_credentials(self.paths, self.secrets, num, email, &successor)?;
            }
            drop(lock);
            Ok(())
        })();
        if let Err(e) = attempt {
            log::warning(format!(
                "Persisting account {num}'s refreshed credential failed; stashing instead. ({e})"
            ));
            match stash_successor("consume-gate-persist-failed") {
                Ok(reason) => {
                    stashed_reason = Some(reason);
                    log::warning(format!(
                        "Persist failed after consuming account {num}'s grant; successor stashed for the next pass."
                    ));
                }
                Err(e) => {
                    stashed_reason = Some("consume-gate-unpersisted");
                    log::error(format!(
                        "Account {num}'s consumed successor could not be persisted or stashed — it survives only for this pass. Fix the storage failure, then re-login and `ccswap add` if the slot strikes. ({e})"
                    ));
                }
            }
        }
        let demoted = stashed_reason.filter(|r| DEMOTING_STASH_REASONS.contains(r));
        RefreshOutcome {
            credentials: Some(outcome_creds),
            error: demoted.map(|_| "transient".to_string()),
            token_account: result.token_account,
            consumed_fp,
            stashed: demoted.is_some_and(|r| r != "consume-gate-unpersisted"),
        }
    }
}

enum Adopt {
    Nothing,
    Adopted(String),
    /// The only copy of a consumed generation cannot be read right now.
    Unreadable,
}

/// Housekeeping: a failed retire leaves one stale row, never a failure.
fn retire(stash: &UnclaimedStash<'_>, id: &str, num: &str) {
    if let Err(e) = stash.remove(id) {
        log::warning(format!(
            "Could not retire account {num}'s stash entry {id}; leaving it for the next pass (`ccswap unclaimed --purge` drops it by hand). ({e})"
        ));
    }
}
