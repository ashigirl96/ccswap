use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::env::{Environment, passwd_user_name};
use crate::paths::Paths;

pub const ACTIVE_OAUTH_SERVICE: &str = "Claude Code-credentials";
pub const MANAGED_KEY_SERVICE: &str = "Claude Code";
pub const BACKUP_SERVICE: &str = "claude-swap";

const NOT_FOUND_RC: i32 = 44;
const SECURITY_TIMEOUT: Duration = Duration::from_secs(5);
const ACTIVE_READ_ATTEMPTS: usize = 2;
const ACTIVE_READ_RETRY_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainError(pub String);

/// The `security` CLI, used the way Claude Code uses it so both read the
/// same items with the same ACLs.
#[derive(Debug, Clone)]
pub struct Keychain {
    security: PathBuf,
    account: String,
}

impl Keychain {
    pub fn new(security: PathBuf, env: &Environment) -> Self {
        Keychain {
            security,
            account: keychain_account_name(env),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    /// `Ok(None)` only for a missing item (exit 44); any other failure is an error.
    pub fn get_password(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<String>, KeychainError> {
        let out = self.run(&["find-generic-password", "-a", account, "-w", "-s", service])?;
        match out.code {
            Some(0) => Ok(Some(
                out.stdout
                    .strip_suffix('\n')
                    .unwrap_or(&out.stdout)
                    .to_string(),
            )),
            Some(NOT_FOUND_RC) => Ok(None),
            code => Err(KeychainError(format!(
                "security find-generic-password failed (rc={}): {}",
                code.map_or("signal".into(), |c| c.to_string()),
                out.stderr.trim()
            ))),
        }
    }

    fn run(&self, args: &[&str]) -> Result<SecurityOutput, KeychainError> {
        let mut child = Command::new(&self.security)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                KeychainError(format!("could not run {}: {e}", self.security.display()))
            })?;
        let deadline = Instant::now() + SECURITY_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| KeychainError(e.to_string()))? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(KeychainError(format!(
                    "security {} timed out after 5.0s",
                    args[0]
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut stdout);
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut stderr);
        }
        Ok(SecurityOutput {
            code: status.code(),
            stdout,
            stderr,
        })
    }
}

struct SecurityOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Claude Code's `getUsername()`: `$USER`, then the passwd entry.
pub fn keychain_account_name(env: &Environment) -> String {
    env.non_empty("USER")
        .or_else(passwd_user_name)
        .unwrap_or_else(|| "claude-code-user".into())
}

/// The Keychain service Claude Code derives for a `CLAUDE_CONFIG_DIR` value.
pub fn profile_service_name(config_dir: &str) -> String {
    let normalized: String = config_dir.nfc().collect();
    let digest = Sha256::digest(normalized.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{ACTIVE_OAUTH_SERVICE}-{}", &hex[..8])
}

/// Outcome of reading Claude Code's active credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCredentials {
    /// The credential, `Some("")` when absent everywhere, `None` when the
    /// plaintext file exists but could not be read.
    pub value: Option<String>,
    /// The OAuth Keychain read failed and nothing else covered it.
    pub keychain_unavailable: bool,
    /// The OAuth Keychain read failed; the value may be a stale generation.
    pub degraded: bool,
}

pub struct SecretStore {
    keychain: Keychain,
    paths: Paths,
    securestorage_dir: Option<String>,
}

impl SecretStore {
    pub fn new(keychain: Keychain, paths: Paths, env: &Environment) -> Self {
        SecretStore {
            keychain,
            paths,
            securestorage_dir: env
                .get("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .map(|v| v.to_string_lossy().into_owned()),
        }
    }

    /// Keychain services that may hold the active OAuth credential, in try order.
    pub fn active_oauth_services(&self) -> Vec<String> {
        if let Some(dir) = &self.securestorage_dir {
            return vec![if dir.is_empty() {
                ACTIVE_OAUTH_SERVICE.to_string()
            } else {
                profile_service_name(dir)
            }];
        }
        let Some(dir) = self.paths.config_dir_env() else {
            return vec![ACTIVE_OAUTH_SERVICE.to_string()];
        };
        let mut services = vec![profile_service_name(dir)];
        if self.paths.active_profile_is_default() {
            services.push(ACTIVE_OAUTH_SERVICE.to_string());
        }
        services
    }

    /// Keychain OAuth item, then `.credentials.json`, then the managed API key.
    pub fn read_active(&self) -> ActiveCredentials {
        let keychain_failed = match self.read_active_oauth_keychain() {
            Ok(Some(value)) => {
                return ActiveCredentials {
                    value: Some(value),
                    keychain_unavailable: false,
                    degraded: false,
                };
            }
            Ok(None) => false,
            Err(_) => true,
        };

        let cred_file = self.paths.credentials_path();
        if cred_file.exists() {
            match std::fs::read_to_string(&cred_file) {
                Err(_) => {
                    return ActiveCredentials {
                        value: None,
                        keychain_unavailable: keychain_failed,
                        degraded: keychain_failed,
                    };
                }
                Ok(text) if !text.trim().is_empty() => {
                    return ActiveCredentials {
                        value: Some(text),
                        keychain_unavailable: false,
                        degraded: keychain_failed,
                    };
                }
                Ok(_) => {}
            }
        }

        let key = self.read_managed_key(keychain_failed);
        if !key.is_empty() {
            return ActiveCredentials {
                value: Some(key),
                keychain_unavailable: false,
                degraded: keychain_failed,
            };
        }
        ActiveCredentials {
            value: Some(String::new()),
            keychain_unavailable: keychain_failed,
            degraded: keychain_failed,
        }
    }

    fn read_active_oauth_keychain(&self) -> Result<Option<String>, KeychainError> {
        for service in self.active_oauth_services() {
            if let Some(value) = self.read_with_retry(&service)?.filter(|v| !v.is_empty()) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn read_with_retry(&self, service: &str) -> Result<Option<String>, KeychainError> {
        let mut last = None;
        for attempt in 0..ACTIVE_READ_ATTEMPTS {
            match self.keychain.get_password(service, self.keychain.account()) {
                Ok(value) => return Ok(value),
                Err(e) => last = Some(e),
            }
            if attempt + 1 < ACTIVE_READ_ATTEMPTS {
                std::thread::sleep(ACTIVE_READ_RETRY_DELAY);
            }
        }
        Err(last.expect("at least one attempt"))
    }

    /// Keychain "Claude Code" for the default profile only, then `primaryApiKey`.
    fn read_managed_key(&self, keychain_failed: bool) -> String {
        if !keychain_failed
            && self.paths.active_profile_is_default()
            && let Ok(Some(value)) = self
                .keychain
                .get_password(MANAGED_KEY_SERVICE, self.keychain.account())
            && !value.is_empty()
        {
            return value;
        }
        read_json_object(&self.paths.global_config_path())
            .and_then(|cfg| {
                cfg.get("primaryApiKey")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }
}

/// A bare `sk-ant-api…` managed key, as opposed to OAuth JSON.
pub fn looks_like_api_key(credentials: &str) -> bool {
    let text = credentials.trim();
    text.starts_with("sk-ant-api") && !text.starts_with('{')
}

/// `claudeAiOauth.accessToken`, when the credential is OAuth JSON.
pub fn extract_access_token(credentials: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(credentials).ok()?;
    value
        .get("claudeAiOauth")?
        .get("accessToken")?
        .as_str()
        .map(str::to_string)
}

fn read_json_object(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_service_name_hashes_the_raw_nfc_value() {
        assert_eq!(
            profile_service_name("/tmp/profile"),
            "Claude Code-credentials-902e721c"
        );
        assert_eq!(
            profile_service_name("/tmp/cafe\u{301}"),
            profile_service_name("/tmp/caf\u{e9}")
        );
        assert_ne!(
            profile_service_name("/tmp/p/"),
            profile_service_name("/tmp/p")
        );
    }

    #[test]
    fn api_key_detection_is_strict() {
        assert!(looks_like_api_key("  sk-ant-api03-abc\n"));
        assert!(!looks_like_api_key("sk-ant-oat01-abc"));
        assert!(!looks_like_api_key(r#"{"claudeAiOauth":{}}"#));
        assert!(!looks_like_api_key(""));
    }

    #[test]
    fn keychain_account_prefers_user() {
        let env = Environment::from_pairs([("USER", "alice")]);
        assert_eq!(keychain_account_name(&env), "alice");
    }
}
