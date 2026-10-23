//! `codex login` into a fresh managed home; Codex does the browser OAuth, ccswap never does.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;

use super::identity::{CodexIdentity, read_identity};
use super::store::{CodexStore, new_account};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    Added {
        number: u32,
        identity: CodexIdentity,
    },
    /// The login was an account already managed; its stored credential now holds the new login.
    Updated {
        number: u32,
        identity: CodexIdentity,
    },
}

/// Where the `codex login` child's stdio goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStdio {
    /// The terminal: Codex prints the URL and the user can paste a device code.
    Inherit,
    /// No terminal, as from the menu bar; Codex still opens the browser.
    Detached,
}

impl CodexStore {
    /// Runs `CODEX_HOME=<new managed home> codex login` and registers whoever logged in.
    ///
    /// On failure the new home is removed and the ledger is left as it was.
    pub fn login(&self, stdio: LoginStdio) -> Result<LoginOutcome> {
        let number = self.reserve_home()?;
        let home = self.paths().managed_home(number);
        let outcome = run_codex_login(&home, stdio).and_then(|()| self.register_login(number));
        if outcome.is_err() {
            let _ = std::fs::remove_dir_all(&home);
        }
        outcome
    }

    /// Creates the next unused managed home under the lock, so a concurrent add or login skips it.
    fn reserve_home(&self) -> Result<u32> {
        self.update(|ledger| {
            let number = self.next_free_number(ledger);
            let home = self.paths().managed_home(number);
            for dir in [
                self.paths().store_dir(),
                self.paths().homes_dir(),
                home.clone(),
            ] {
                fsutil::ensure_private_dir(&dir).map_err(|e| {
                    Error::new(
                        ErrorKind::CredentialWriteError,
                        format!("Could not create {}: {e}", dir.display()),
                    )
                })?;
            }
            Ok(number)
        })
    }

    fn register_login(&self, number: u32) -> Result<LoginOutcome> {
        let auth = self.read_managed(number)?.ok_or_else(|| {
            Error::new(
                ErrorKind::CredentialError,
                "codex login finished without writing auth.json. Nothing was added.",
            )
        })?;
        let identity = read_identity(&auth)?;
        self.update(|ledger| {
            if let Some(existing) = ledger.find_by_account_id(&identity.account_id) {
                self.write_managed(existing, &auth)?;
                let _ = std::fs::remove_dir_all(self.paths().managed_home(number));
                return Ok(LoginOutcome::Updated {
                    number: existing,
                    identity: identity.clone(),
                });
            }
            self.write_managed(number, &auth)?;
            ledger.insert(number, new_account(&identity));
            Ok(LoginOutcome::Added {
                number,
                identity: identity.clone(),
            })
        })
    }
}

fn run_codex_login(home: &Path, stdio: LoginStdio) -> Result<()> {
    let mut command = Command::new("codex");
    command.arg("login").env("CODEX_HOME", home);
    if stdio == LoginStdio::Detached {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    let status = command.status().map_err(|e| {
        Error::new(
            ErrorKind::CredentialError,
            format!("Could not run 'codex login': {e}. Is the Codex CLI installed?"),
        )
    })?;
    if !status.success() {
        let how = status.code().map_or_else(
            || "was interrupted".to_string(),
            |c| format!("exited with {c}"),
        );
        return Err(Error::new(
            ErrorKind::CredentialError,
            format!("'codex login' {how}. Nothing was added."),
        ));
    }
    Ok(())
}
