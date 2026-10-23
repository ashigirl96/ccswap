use std::path::PathBuf;
use std::time::Duration;

use crate::env::Environment;

use super::app_server;

/// A program `switch` runs (as `<hook> ccswap-switch-hook`, with `CODEX_HOME` set) between
/// writing the live login back and re-reading it.
pub const SWITCH_HOOK_ENV: &str = "CCSWAP_TEST_CODEX_SWITCH_HOOK";
/// Milliseconds a `codex app-server` pass may take.
pub const APP_SERVER_TIMEOUT_ENV: &str = "CCSWAP_TEST_CODEX_APP_SERVER_TIMEOUT_MS";

/// Debug-build-only knobs that let tests rewrite the live login mid-switch and time
/// out an app-server pass without waiting for the real limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexOverrides {
    pub switch_hook: Option<PathBuf>,
    pub app_server_timeout: Duration,
}

impl CodexOverrides {
    pub fn new(env: &Environment) -> Self {
        let debug = |name: &str| env.non_empty(name).filter(|_| cfg!(debug_assertions));
        CodexOverrides {
            switch_hook: debug(SWITCH_HOOK_ENV).map(PathBuf::from),
            app_server_timeout: debug(APP_SERVER_TIMEOUT_ENV)
                .and_then(|ms| ms.parse().ok())
                .map(Duration::from_millis)
                .unwrap_or(app_server::DEFAULT_TIMEOUT),
        }
    }
}
