//! `ccswap codex switch`: write the live `auth.json` back to its owner, then replace it
//! with the target's, all under the store `.lock`.

use std::path::{Path, PathBuf};

use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::fsutil;

use super::identity::read_identity;
use super::ledger::Ledger;
use super::resolve::resolve_account;
use super::store::CodexStore;

/// Codex rewrites `auth.json` non-atomically when it refreshes; past this many
/// changed re-reads the switch gives up rather than race it.
pub const MAX_SETTLE_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchedAccount {
    /// `None` for a live login that no managed account owns.
    pub number: Option<u32>,
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    Switched,
    AlreadyActive,
    OnlyOneAccount,
}

impl SwitchReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchReason::Switched => "switched",
            SwitchReason::AlreadyActive => "already-active",
            SwitchReason::OnlyOneAccount => "only-one-account",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchStrategy {
    /// A named target.
    Direct,
    /// The next account in sequence.
    Rotation,
}

impl SwitchStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchStrategy::Direct => "direct",
            SwitchStrategy::Rotation => "rotation",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    pub switched: bool,
    pub reason: SwitchReason,
    pub strategy: SwitchStrategy,
    pub from: Option<SwitchedAccount>,
    pub to: SwitchedAccount,
    /// Where an unmanaged live login was set aside.
    pub unclaimed: Vec<PathBuf>,
    /// `<CODEX_HOME>/app-server-daemon/`, when it exists.
    pub daemon_dir: Option<PathBuf>,
}

impl CodexStore {
    /// `identifier` is `<num|alias|email>`; `None` rotates to the next account.
    pub fn switch(&self, identifier: Option<&str>) -> Result<SwitchOutcome> {
        ensure_file_credentials_store(&self.paths().live_config())?;
        let daemon_dir =
            Some(self.paths().live_home().join("app-server-daemon")).filter(|dir| dir.is_dir());
        self.update(|ledger| {
            if ledger.accounts.is_empty() {
                return Err(Error::config(
                    "No Codex accounts are managed yet. Run 'ccswap codex add' first.",
                ));
            }
            let live_owner = self
                .read_live()?
                .and_then(|bytes| read_identity(&bytes).ok())
                .and_then(|id| ledger.find_by_account_id(&id.account_id));
            let (target, strategy) = match identifier {
                Some(identifier) => (resolve_account(ledger, identifier)?, SwitchStrategy::Direct),
                None => (next_in_sequence(ledger, live_owner), SwitchStrategy::Rotation),
            };
            let to = SwitchedAccount {
                number: Some(target),
                email: ledger.accounts[&target].email.clone(),
            };
            if live_owner == Some(target) {
                let reason = if identifier.is_none() && ledger.accounts.len() < 2 {
                    SwitchReason::OnlyOneAccount
                } else {
                    SwitchReason::AlreadyActive
                };
                return Ok(SwitchOutcome {
                    switched: false,
                    reason,
                    strategy,
                    from: Some(to.clone()),
                    to,
                    unclaimed: Vec::new(),
                    daemon_dir: None,
                });
            }
            let target_bytes = self.read_managed(target)?.ok_or_else(|| {
                Error::new(
                    ErrorKind::CredentialReadError,
                    format!(
                        "Codex account {target} has no stored login at {}. Log in to it again and re-add it.",
                        self.paths().managed_auth(target).display()
                    ),
                )
            })?;
            let (from, unclaimed) = self.settle_live(ledger)?;
            let live_home = self.paths().live_home();
            fsutil::atomic_write_private(live_home, &self.paths().live_auth(), &target_bytes)
                .map_err(|e| {
                    Error::new(
                        ErrorKind::CredentialWriteError,
                        format!("Could not write {}: {e}", self.paths().live_auth().display()),
                    )
                })?;
            ledger.active = Some(target);
            Ok(SwitchOutcome {
                switched: true,
                reason: SwitchReason::Switched,
                strategy,
                from,
                to,
                unclaimed,
                daemon_dir: daemon_dir.clone(),
            })
        })
    }

    /// Saves the live login to its owner (or `unclaimed/`) until a re-read shows it unchanged.
    fn settle_live(&self, ledger: &Ledger) -> Result<(Option<SwitchedAccount>, Vec<PathBuf>)> {
        let mut unclaimed = Vec::new();
        let mut live = self.read_live()?;
        for _ in 0..MAX_SETTLE_ATTEMPTS {
            let Some(bytes) = &live else {
                return Ok((None, unclaimed));
            };
            let identity = read_identity(bytes).ok();
            let owner = identity
                .as_ref()
                .and_then(|id| ledger.find_by_account_id(&id.account_id));
            match owner {
                Some(number) => self.write_managed(number, bytes)?,
                None => unclaimed.push(self.stash_unclaimed(bytes)?),
            }
            self.run_switch_hook();
            let again = self.read_live()?;
            if again == live {
                let from = SwitchedAccount {
                    number: owner,
                    email: identity.map(|id| id.email).unwrap_or_default(),
                };
                return Ok((Some(from), unclaimed));
            }
            live = again;
        }
        Err(Error::new(
            ErrorKind::CredentialError,
            format!(
                "{} kept changing while ccswap saved it (Codex may be refreshing its login). Nothing was switched; try again.",
                self.paths().live_auth().display()
            ),
        ))
    }

    fn run_switch_hook(&self) {
        if let Some(hook) = &self.paths().overrides().switch_hook {
            let _ = std::process::Command::new(hook)
                .arg("ccswap-switch-hook")
                .env("CODEX_HOME", self.paths().live_home())
                .status();
        }
    }

    fn stash_unclaimed(&self, bytes: &[u8]) -> Result<PathBuf> {
        let dir = self.paths().unclaimed_dir();
        let io = |e: std::io::Error| {
            Error::new(
                ErrorKind::CredentialWriteError,
                format!("Could not write {}: {e}", dir.display()),
            )
        };
        for d in [self.paths().store_dir(), dir.clone()] {
            fsutil::ensure_private_dir(&d).map_err(io)?;
        }
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let path = (0..)
            .map(|n| match n {
                0 => dir.join(format!("auth-{stamp}.json")),
                n => dir.join(format!("auth-{stamp}-{n}.json")),
            })
            .find(|p| !p.exists())
            .expect("an unused name");
        fsutil::atomic_write_private(&dir, &path, bytes).map_err(io)?;
        Ok(path)
    }
}

/// The account after `anchor` (the live owner, else the ledger's active) in sequence order.
fn next_in_sequence(ledger: &Ledger, live_owner: Option<u32>) -> u32 {
    let order: Vec<u32> = ledger.ordered().into_iter().map(|(n, _)| n).collect();
    let anchor = live_owner.or(ledger.active);
    match anchor.and_then(|a| order.iter().position(|n| *n == a)) {
        Some(at) => order[(at + 1) % order.len()],
        None => order[0],
    }
}

/// Only `file` (the default) keeps the login in `auth.json`, which is all a switch rewrites.
fn ensure_file_credentials_store(config: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Error::config(format!(
                "Could not read {}: {e}",
                config.display()
            )));
        }
    };
    match credentials_store(&text) {
        None => Ok(()),
        Some(mode) if mode == "file" => Ok(()),
        Some(mode) => Err(Error::config(format!(
            "Codex keeps its login in '{mode}' mode (cli_auth_credentials_store in {}). ccswap codex switch supports only \"file\"; nothing was changed.",
            config.display()
        ))),
    }
}

/// The top-level `cli_auth_credentials_store` value; keys under a `[table]` are not it.
fn credentials_store(toml: &str) -> Option<String> {
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return None;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().trim_matches(['"', '\'']) != "cli_auth_credentials_store" {
            continue;
        }
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default(),
            _ => value.split('#').next().unwrap_or_default().trim(),
        };
        return Some(value.to_string());
    }
    None
}
