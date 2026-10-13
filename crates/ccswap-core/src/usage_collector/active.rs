//! The active slot's usage fetch (upstream `_fetch_active_usage` and
//! `_resync_rotated_backup`). An expired or server-rejected active token is
//! refreshed under Claude Code's own lock protocol (locks → re-read →
//! decide → POST → persist → release), which a live Claude Code serializes
//! against and then adopts.
//!
//! Two invariants: a live credential is consumed or written into the slot
//! backup only when its lineage is attributed to the slot (backup lineage,
//! a profile-oracle verdict, or our own POST); and a consumed generation is
//! never discarded — the successor is written to both stores, each allowed
//! to fail alone.

use std::time::Duration;

use serde_json::{Map, Value};

use super::{AccountInfo, Fetcher, record_of};
use crate::anthropic_client::{credential_fingerprint, extract_oauth_data, is_oauth_token_expired};
use crate::consume_gate::{consume_lock_path, write_account_credentials};
use crate::error::{ErrorKind, Result};
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock, claude_config_lock, claude_credentials_lock};
use crate::log;
use crate::probe_verdicts;
use crate::secret_store::extract_access_token;
use crate::store;
use crate::usage::{Sentinel, truthy};
use crate::usage_store::FetchRecord;

/// Bounded well inside a concurrent switch's 10 s store-lock budget, since
/// this POST runs under that lock.
const LOCKED_REFRESH_TIMEOUT: Duration = Duration::from_secs(6);

enum Locked {
    Record(FetchRecord),
    Working(String),
}

fn has_token_pair(oauth: &Map<String, Value>) -> bool {
    oauth.get("accessToken").is_some_and(truthy) && oauth.get("refreshToken").is_some_and(truthy)
}

fn expiry(oauth: Option<&Map<String, Value>>) -> f64 {
    oauth
        .and_then(|o| o.get("expiresAt"))
        .filter(|v| truthy(v))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn fingerprint(credentials: &str) -> String {
    credential_fingerprint(credentials).unwrap_or_default()
}

/// A token expired locally gets the sentinel; a locally valid one the
/// server rejected keeps its 401 so the store paces retries.
fn defer(rejected: &Option<FetchRecord>) -> FetchRecord {
    rejected
        .clone()
        .unwrap_or(FetchRecord::sentinel(Sentinel::TokenExpired))
}

impl Fetcher<'_> {
    pub(super) fn fetch_active(&self, info: &AccountInfo) -> FetchRecord {
        let (num, email, creds) = (&info.num, &info.email, &info.creds);
        let Some(oauth) =
            extract_oauth_data(creds).filter(|o| o.get("accessToken").is_some_and(truthy))
        else {
            return FetchRecord::sentinel(Sentinel::NoCredentials);
        };

        let mut rejected = None;
        if !is_oauth_token_expired(oauth.get("expiresAt")) {
            let outcome = self
                .client
                .try_fetch_usage_for_account(num, email, creds, true, None);
            if outcome.error.as_deref() != Some("http-401") {
                if outcome.usage.is_some() {
                    if !self.degraded {
                        self.resync_rotated_backup(info);
                    }
                    if probe_verdicts::get(self.paths, num, email, &fingerprint(creds))
                        == Some(false)
                    {
                        return FetchRecord::sentinel(Sentinel::ForeignCredential);
                    }
                }
                return record_of(outcome);
            }
            rejected = Some(FetchRecord::error(
                outcome.error.unwrap_or_default(),
                outcome.retry_after_s,
            ));
        }

        // A degraded read may be a stale generation that the backup agrees
        // with; consuming its grant would strike a live account.
        if self.degraded {
            return rejected.unwrap_or(FetchRecord::sentinel(Sentinel::KeychainUnavailable));
        }
        if self.unmirrored {
            log::warning(format!(
                "CLAUDE_SECURESTORAGE_CONFIG_DIR is set; cswap mirrors it when capturing a credential but not when refreshing one, so refusing to refresh account {num}'s active credential (unset the variable or run from a normal shell)."
            ));
            return FetchRecord::error("store-unmirrored", None);
        }

        let backup = self.secrets.read_backup(num, email).value;
        let backup_usable = extract_oauth_data(&backup).is_some_and(|o| has_token_pair(&o));
        let attributable =
            *creds == backup || credential_fingerprint(creds) == credential_fingerprint(&backup);
        if !attributable && !backup_usable {
            if probe_verdicts::first_warning(self.paths, num, email, "unattributable") {
                log::warning(format!(
                    "Active credential does not match Account-{num}'s stored backup and the backup is unusable; cannot refresh (provenance unknown)."
                ));
            }
            return defer(&rejected);
        }
        probe_verdicts::clear_warning(self.paths, num, email, "unattributable");

        let working = match self.refresh_locked(info, &backup, backup_usable, &rejected) {
            Ok(Locked::Working(working)) => working,
            Ok(Locked::Record(record)) => return record,
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::LockError | ErrorKind::ClaudeCodeLockTimeout
                ) =>
            {
                log::info(format!(
                    "Credential locks held elsewhere; deferring the active-token refresh for account {num} to the next pass."
                ));
                return defer(&rejected);
            }
            Err(e) => {
                log::warning(format!(
                    "Active-token refresh for account {num} failed unexpectedly; deferring to the next pass. ({e})"
                ));
                return defer(&rejected);
            }
        };
        record_of(
            self.client
                .try_fetch_usage_for_account(num, email, &working, true, None),
        )
    }

    /// The locked section: consume lock (shared with the gate, which may
    /// POST this slot's backup too), store lock, Claude Code's pair. The
    /// config lock covers only the live write.
    fn refresh_locked(
        &self,
        info: &AccountInfo,
        backup: &str,
        backup_usable: bool,
        rejected: &Option<FetchRecord>,
    ) -> Result<Locked> {
        let (num, email, creds) = (&info.num, &info.email, &info.creds);
        let _consume = StoreLock::acquire(&consume_lock_path(self.paths, num), STORE_LOCK_TIMEOUT)?;
        let _store = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let _claude = claude_credentials_lock(self.paths, None)?;

        let Some(live) = self.secrets.read_active().value else {
            return Ok(Locked::Record(defer(rejected)));
        };
        let live_oauth = if live.is_empty() {
            None
        } else {
            extract_oauth_data(&live)
        };
        if !live_identity_matches(self, email, &info.org_uuid) {
            return Ok(Locked::Record(defer(rejected)));
        }
        let backup_fp = credential_fingerprint(backup);
        let backup_oauth = extract_oauth_data(backup);

        let rotated_elsewhere = live_oauth.as_ref().is_some_and(|lo| {
            live != *creds && has_token_pair(lo) && !is_oauth_token_expired(lo.get("expiresAt"))
        });
        if rotated_elsewhere {
            // Someone (a live Claude Code) already rotated it: adopt, consume
            // nothing, and resync the backup only for an attributed lineage.
            let verdict = probe_verdicts::get(self.paths, num, email, &fingerprint(&live));
            if verdict == Some(false) {
                return Ok(Locked::Record(FetchRecord::sentinel(
                    Sentinel::ForeignCredential,
                )));
            }
            if verdict == Some(true) || credential_fingerprint(&live) == backup_fp {
                if write_account_credentials(self.paths, self.secrets, num, email, &live).is_err() {
                    log::warning(format!(
                        "Backup resync after adopting a rotated credential failed for account {num}; the next expiry may refuse to refresh until a switch resyncs it."
                    ));
                }
            } else {
                log::debug(format!(
                    "Adopted a rotated live credential for account {num} without a lineage verdict; backup resync deferred to the next fresh pass's oracle check."
                ));
            }
            return Ok(Locked::Working(live));
        }

        let usable_backup_or = |fallback: &str| {
            if backup_usable {
                backup.to_string()
            } else {
                fallback.to_string()
            }
        };
        let refresh_input = if live_oauth.is_some() && credential_fingerprint(&live) == backup_fp {
            if live_oauth
                .as_ref()
                .is_some_and(|o| o.get("refreshToken").is_some_and(truthy))
            {
                live.clone()
            } else {
                usable_backup_or(creds)
            }
        } else if live.is_empty() {
            usable_backup_or(creds)
        } else if live == *creds {
            // Expiry ordering selects a candidate; only an ownership verdict
            // licenses consuming a live lineage the backup does not share.
            let newer_live = live_oauth
                .as_ref()
                .is_some_and(|o| o.get("refreshToken").is_some_and(truthy))
                && expiry(live_oauth.as_ref()) > expiry(backup_oauth.as_ref());
            if newer_live {
                if probe_verdicts::get(self.paths, num, email, &fingerprint(&live)) == Some(true) {
                    live.clone()
                } else {
                    if probe_verdicts::first_warning(self.paths, num, email, "expiry-unattributed")
                    {
                        log::warning(format!(
                            "Live credential is newer than Account-{num}'s backup but its ownership is unverified; refresh deferred to Claude Code's next use."
                        ));
                    }
                    return Ok(Locked::Record(defer(rejected)));
                }
            } else {
                usable_backup_or(creds)
            }
        } else {
            // Live moved mid-flight to bytes that are neither what the pass
            // read nor the backup's lineage: another actor is mid-write.
            return Ok(Locked::Record(defer(rejected)));
        };

        let input_oauth = extract_oauth_data(&refresh_input);
        let restore = refresh_input == backup
            && backup_usable
            && rejected.is_none()
            && input_oauth
                .as_ref()
                .is_some_and(|o| !is_oauth_token_expired(o.get("expiresAt")));
        let working = if restore {
            // A prior locked refresh persisted the backup but its live write
            // failed: restore it, consuming nothing.
            backup.to_string()
        } else {
            let result = self
                .client
                .try_refresh_oauth_credentials(&refresh_input, LOCKED_REFRESH_TIMEOUT);
            let dead = matches!(
                result.error.as_deref(),
                Some("invalid_grant" | "no_refresh_token")
            ) || (result.error.is_none() && result.credentials.is_none());
            if dead {
                // A source that moved during our POST means we consumed a
                // superseded copy: evidence about our bytes, not the slot.
                let source_now = if refresh_input == backup {
                    self.secrets.read_backup(num, email).value
                } else {
                    self.secrets.read_active().value.unwrap_or_default()
                };
                if !source_now.is_empty()
                    && credential_fingerprint(&source_now) != credential_fingerprint(&refresh_input)
                {
                    return Ok(Locked::Record(FetchRecord::error("refresh-failed", None)));
                }
                return Ok(Locked::Record(FetchRecord {
                    error: Some(result.error.unwrap_or_else(|| "invalid_grant".into())),
                    struck_fp: credential_fingerprint(&refresh_input),
                    ..Default::default()
                }));
            }
            let Some(working) = result.credentials.filter(|_| result.error.is_none()) else {
                return Ok(Locked::Record(FetchRecord::error("refresh-failed", None)));
            };
            probe_verdicts::record(self.paths, num, email, &fingerprint(&working), true);
            probe_verdicts::clear_warning(self.paths, num, email, "expiry-unattributed");
            working
        };

        let mut backup_ok = true;
        if !restore
            && write_account_credentials(self.paths, self.secrets, num, email, &working).is_err()
        {
            backup_ok = false;
            log::warning(format!(
                "Backup write failed after a consumed refresh for account {num}; attempting the active store."
            ));
        }
        let live_written = claude_config_lock(self.paths, None)
            .and_then(|_config| self.secrets.write_active(&working).map(|_| ()));
        if live_written.is_err() {
            log::warning(format!(
                "Active-store write failed after a {} for account {num}{}.",
                if restore {
                    "backup restore"
                } else {
                    "consumed refresh"
                },
                if backup_ok {
                    ""
                } else {
                    "; the rotated credential was NOT persisted anywhere — re-login may be required"
                }
            ));
            return Ok(Locked::Record(FetchRecord::sentinel(
                Sentinel::TokenExpired,
            )));
        }
        Ok(Locked::Working(working))
    }

    /// After the server accepted a live credential of another lineage than
    /// the backup (Claude Code rotated during normal use), resync the backup
    /// — but only once the profile endpoint attributes that lineage to the
    /// slot, probed here before any lock. Best effort; never fails.
    fn resync_rotated_backup(&self, info: &AccountInfo) {
        let (num, email, creds) = (&info.num, &info.email, &info.creds);
        if !extract_oauth_data(creds).is_some_and(|o| has_token_pair(&o)) {
            return;
        }
        let backup = self.secrets.read_backup(num, email).value;
        if !backup.is_empty() && credential_fingerprint(creds) == credential_fingerprint(&backup) {
            return;
        }
        let fp = fingerprint(creds);
        match probe_verdicts::get(self.paths, num, email, &fp) {
            Some(false) => return,
            Some(true) => {}
            None => {
                let token = extract_access_token(creds).unwrap_or_default();
                let Some(resolved) = self.client.fetch_profile(&token) else {
                    log::debug(format!(
                        "Ownership probe for account {num}'s drifted live credential failed; resync skipped this pass."
                    ));
                    return;
                };
                let Some(owned) = resolved_matches_slot_identity(self, num, &resolved) else {
                    log::debug(format!(
                        "Ownership of account {num}'s drifted live credential is unverifiable (no stored uuid, partial profile); resync skipped this pass."
                    ));
                    return;
                };
                probe_verdicts::record(self.paths, num, email, &fp, owned);
                if !owned {
                    if probe_verdicts::first_warning(self.paths, num, email, "resync") {
                        log::warning(format!(
                            "Live credential resolves to a different account than Account-{num}'s identity; backup left untouched (foreign credential under a stale config)."
                        ));
                    }
                    return;
                }
                probe_verdicts::clear_warning(self.paths, num, email, "resync");
            }
        }
        let resynced = (|| -> Result<bool> {
            let _store = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
            let _claude = claude_credentials_lock(self.paths, None)?;
            if !live_identity_matches(self, email, &info.org_uuid)
                || probe_verdicts::get(self.paths, num, email, &fp) != Some(true)
            {
                return Ok(false);
            }
            let live = self.secrets.read_active().value.unwrap_or_default();
            let same_pair = !live.is_empty()
                && extract_oauth_data(&live).is_some_and(|o| has_token_pair(&o))
                && credential_fingerprint(&live) == credential_fingerprint(creds);
            if !same_pair {
                return Ok(false);
            }
            write_account_credentials(self.paths, self.secrets, num, email, &live)?;
            Ok(true)
        })();
        match resynced {
            Ok(true) => log::info(format!(
                "Resynced account {num}'s backup to the rotated live credential (rotation completed outside a collect pass)."
            )),
            Ok(false) => {}
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::LockError | ErrorKind::ClaudeCodeLockTimeout
                ) => {}
            Err(e) => log::warning(format!(
                "Backup resync for account {num} failed; the recovery branch's newer-generation check still guards the next expiry. ({e})"
            )),
        }
    }
}

/// The live config names `(email, org)` right now: a switch or `/login`
/// in the gap means the live store is no longer this slot's.
fn live_identity_matches(fetcher: &Fetcher<'_>, email: &str, org_uuid: &str) -> bool {
    store::current_identity(fetcher.paths)
        .is_some_and(|i| i.email == email && i.organization_uuid == org_uuid)
}

/// Whether an oracle-resolved identity is the slot's account: `Some(true)`
/// same, `Some(false)` definitively another (cacheable), `None`
/// unverifiable. Uuid first; a uuid-less slot is affirmed only by an exact
/// `(email, org)` match with the org present, and then gets the uuid.
pub(crate) fn resolved_matches_slot_identity(
    fetcher: &Fetcher<'_>,
    num: &str,
    resolved: &crate::anthropic_client::Profile,
) -> Option<bool> {
    let data = store::read_sequence(fetcher.paths)
        .ok()
        .flatten()
        .unwrap_or_default();
    let own = store::slot_identity(&data, num);
    let r_uuid = resolved.uuid.trim();
    let r_org = resolved.organization_uuid.as_deref();
    if !r_uuid.is_empty() && !own.uuid.is_empty() {
        return Some(
            r_uuid == own.uuid
                && (r_org.is_none_or(str::is_empty)
                    || own.organization_uuid.is_empty()
                    || r_org == Some(own.organization_uuid.as_str())),
        );
    }
    let r_email = resolved.email.as_deref().filter(|e| !e.is_empty());
    if let Some(r_email) = r_email
        && r_email == own.email
    {
        let r_org = r_org?;
        if r_org == own.organization_uuid {
            if let Err(e) =
                store::backfill_account_uuid(fetcher.paths, num, r_uuid, Some((r_email, r_org)))
            {
                log::debug(format!("uuid backfill failed for account {num}: {e}"));
            }
            return Some(true);
        }
        return Some(false);
    }
    if r_email.is_some() && r_org.is_some() {
        return Some(false);
    }
    None
}
