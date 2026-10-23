use std::path::PathBuf;

use crate::env::Environment;
use crate::paths::Paths;

use super::program::CodexProgram;

pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

#[derive(Debug, Clone)]
pub struct CodexPaths {
    live_home: PathBuf,
    backup_root: PathBuf,
    program: CodexProgram,
}

impl CodexPaths {
    pub fn new(env: &Environment) -> Self {
        let paths = Paths::new(env);
        CodexPaths {
            live_home: env
                .non_empty(CODEX_HOME_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.home().join(".codex")),
            backup_root: paths.backup_root(),
            program: CodexProgram::resolve(env),
        }
    }

    /// The `CODEX_HOME` that the user's `codex` reads.
    pub fn live_home(&self) -> &PathBuf {
        &self.live_home
    }

    pub fn codex_program(&self) -> &CodexProgram {
        &self.program
    }

    pub fn live_auth(&self) -> PathBuf {
        self.live_home.join("auth.json")
    }

    pub fn live_config(&self) -> PathBuf {
        self.live_home.join("config.toml")
    }

    pub fn store_dir(&self) -> PathBuf {
        self.backup_root.join("codex")
    }

    pub fn ledger_file(&self) -> PathBuf {
        self.store_dir().join("accounts.json")
    }

    pub fn homes_dir(&self) -> PathBuf {
        self.store_dir().join("homes")
    }

    /// The account's own `CODEX_HOME`, holding only `auth.json`.
    pub fn managed_home(&self, number: u32) -> PathBuf {
        self.homes_dir().join(number.to_string())
    }

    pub fn managed_auth(&self, number: u32) -> PathBuf {
        self.managed_home(number).join("auth.json")
    }

    pub fn unclaimed_dir(&self) -> PathBuf {
        self.store_dir().join("unclaimed")
    }

    pub fn usage_cache_dir(&self) -> PathBuf {
        self.store_dir().join("cache")
    }

    /// Shared with the Claude store so CLI, menubar and widget serialize on one lock.
    pub fn store_lock_file(&self) -> PathBuf {
        self.backup_root.join(".lock")
    }
}
