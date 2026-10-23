use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::store;

use super::identity::{CodexIdentity, read_identity};
use super::ledger::{Ledger, LedgerAccount};
use super::paths::CodexPaths;
use super::resolve::resolve_account;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    Added {
        number: u32,
        identity: CodexIdentity,
    },
    AlreadyManaged {
        number: u32,
        identity: CodexIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedCodexAccount {
    pub number: u32,
    pub email: String,
    pub alias: Option<String>,
    pub plan: String,
    pub account_id: String,
    /// Owns the live `auth.json`.
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexListing {
    pub accounts: Vec<ListedCodexAccount>,
}

impl CodexListing {
    pub fn active_number(&self) -> Option<u32> {
        self.accounts.iter().find(|a| a.active).map(|a| a.number)
    }
}

pub struct CodexStore {
    paths: CodexPaths,
}

impl CodexStore {
    pub fn new(paths: CodexPaths) -> Self {
        CodexStore { paths }
    }

    pub fn paths(&self) -> &CodexPaths {
        &self.paths
    }

    /// A snapshot without the lock; rewrite through [`update`](Self::update).
    pub fn ledger(&self) -> Result<Ledger> {
        Ledger::load(&self.paths.ledger_file())
    }

    /// Read-modify-write of the ledger under the store `.lock`; unchanged ledgers are not rewritten.
    pub fn update<T>(&self, edit: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        let before = self.ledger()?;
        let mut ledger = before.clone();
        let out = edit(&mut ledger)?;
        if ledger != before {
            self.write_ledger(&ledger)?;
        }
        Ok(out)
    }

    /// The live `auth.json` bytes; `None` when there is no login.
    pub fn read_live(&self) -> Result<Option<Vec<u8>>> {
        read_optional(&self.paths.live_auth())
    }

    pub fn read_managed(&self, number: u32) -> Result<Option<Vec<u8>>> {
        read_optional(&self.paths.managed_auth(number))
    }

    /// Stores `bytes` unchanged as the account's `auth.json` (dir 0700, file 0600, atomic).
    pub fn write_managed(&self, number: u32, bytes: &[u8]) -> Result<()> {
        let home = self.paths.managed_home(number);
        let io = |e: std::io::Error| {
            Error::new(
                ErrorKind::CredentialWriteError,
                format!("Could not write {}: {e}", home.display()),
            )
        };
        for dir in [self.paths.store_dir(), self.paths.homes_dir(), home.clone()] {
            fsutil::ensure_private_dir(&dir).map_err(io)?;
        }
        fsutil::atomic_write_private(&home, &self.paths.managed_auth(number), bytes).map_err(io)
    }

    /// Registers whoever owns the live `auth.json`; an account already managed is left as is.
    pub fn add_live(&self) -> Result<AddOutcome> {
        let live = self.read_live()?.ok_or_else(|| {
            Error::new(
                ErrorKind::CredentialError,
                format!(
                    "No Codex login found at {}. Run 'codex login' first.",
                    self.paths.live_auth().display()
                ),
            )
        })?;
        let identity = read_identity(&live)?;
        self.update(|ledger| {
            if let Some(number) = ledger.find_by_account_id(&identity.account_id) {
                return Ok(AddOutcome::AlreadyManaged {
                    number,
                    identity: identity.clone(),
                });
            }
            let number = self.next_free_number(ledger);
            self.write_managed(number, &live)?;
            ledger.insert(number, new_account(&identity));
            ledger.active = Some(number);
            Ok(AddOutcome::Added {
                number,
                identity: identity.clone(),
            })
        })
    }

    /// Drops the ledger row and the managed home, never the live `auth.json`.
    /// Resolved under the lock, so a row another process removed meanwhile is reported, not recreated.
    pub fn remove(&self, identifier: &str) -> Result<(u32, LedgerAccount)> {
        self.update(|ledger| {
            let number = resolve_account(ledger, identifier)?;
            let account = ledger
                .accounts
                .remove(&number)
                .expect("resolve_account returns a present row");
            ledger.sequence.retain(|n| *n != number);
            if ledger.active == Some(number) {
                ledger.active = None;
            }
            let home = self.paths.managed_home(number);
            match std::fs::remove_dir_all(&home) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::CredentialWriteError,
                        format!("Could not remove {}: {e}", home.display()),
                    ));
                }
            }
            Ok((number, account))
        })
    }

    /// Sets the alias, or clears it when `alias` is empty; returns the number and the stored alias.
    pub fn set_alias(&self, identifier: &str, alias: &str) -> Result<(u32, Option<String>)> {
        let normalized = if alias.trim().is_empty() {
            None
        } else {
            Some(store::normalize_alias(alias)?)
        };
        self.update(|ledger| {
            let number = resolve_account(ledger, identifier)?;
            if let Some(wanted) = &normalized
                && let Some((other, _)) = ledger
                    .accounts
                    .iter()
                    .find(|(n, a)| **n != number && a.alias.as_deref() == Some(wanted.as_str()))
            {
                return Err(Error::config(format!(
                    "Alias '{wanted}' is already used by Codex account {other}"
                )));
            }
            if let Some(account) = ledger.accounts.get_mut(&number) {
                account.alias = normalized.clone();
            }
            Ok((number, normalized.clone()))
        })
    }

    pub fn list(&self) -> Result<CodexListing> {
        let ledger = self.ledger()?;
        let live_owner = self
            .read_live()
            .ok()
            .flatten()
            .and_then(|bytes| read_identity(&bytes).ok())
            .map(|id| id.account_id);
        let accounts = ledger
            .ordered()
            .into_iter()
            .map(|(number, a)| ListedCodexAccount {
                number,
                email: a.email.clone(),
                alias: a.alias.clone(),
                plan: a.plan.clone(),
                account_id: a.account_id.clone(),
                active: live_owner.as_deref() == Some(a.account_id.as_str()),
            })
            .collect();
        Ok(CodexListing { accounts })
    }

    /// The ledger's next number, skipping homes that exist without a row (a `codex login` in progress).
    pub(super) fn next_free_number(&self, ledger: &Ledger) -> u32 {
        let mut number = ledger.next_number();
        while self.paths.managed_home(number).exists() {
            number += 1;
        }
        number
    }

    fn write_ledger(&self, ledger: &Ledger) -> Result<()> {
        fsutil::ensure_private_dir(&self.paths.store_dir()).map_err(|e| {
            Error::config(format!(
                "Could not create {}: {e}",
                self.paths.store_dir().display()
            ))
        })?;
        fsutil::write_json_pid_tmp(&self.paths.ledger_file(), &ledger.to_json())
    }
}

pub fn new_account(identity: &CodexIdentity) -> LedgerAccount {
    LedgerAccount {
        email: identity.email.clone(),
        account_id: identity.account_id.clone(),
        plan: identity.plan.clone(),
        alias: None,
        added: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        extra: Default::default(),
    }
}

fn read_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::new(
            ErrorKind::CredentialReadError,
            format!("Could not read {}: {e}", path.display()),
        )),
    }
}
