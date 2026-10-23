use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};

use super::identity::{CodexIdentity, read_identity};
use super::ledger::{Ledger, LedgerAccount};
use super::paths::CodexPaths;

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
            let number = ledger.next_number();
            self.write_managed(number, &live)?;
            ledger.insert(number, new_account(&identity));
            ledger.active = Some(number);
            Ok(AddOutcome::Added {
                number,
                identity: identity.clone(),
            })
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
