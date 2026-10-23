//! Writing Claude Code's active credential: one auth axis at a time, OAuth
//! in the Keychain (file fallback) or a managed API key.

use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::fsutil;
use ccswap_core::pyjson;
use serde_json::{Map, Value};

use super::{ACTIVE_OAUTH_SERVICE, MANAGED_KEY_SERVICE, SecretStore, looks_like_api_key};

/// Where an active-credential write landed; decides the post-switch hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveBackend {
    Keychain,
    File,
}

impl SecretStore {
    /// Activate `credentials`: an `sk-ant-api…` key becomes the managed key
    /// and the OAuth login is cleared; anything else is written as the OAuth
    /// login and the managed key is cleared.
    pub fn write_active(&self, credentials: &str) -> Result<ActiveBackend> {
        if looks_like_api_key(credentials) {
            self.write_managed_key(credentials.trim())
        } else {
            let backend = self.write_active_oauth(credentials)?;
            self.clear_managed_key();
            Ok(backend)
        }
    }

    /// Where the next active write would go, for when none happened.
    pub fn routing_backend(&self) -> ActiveBackend {
        if self.use_keychain() {
            ActiveBackend::Keychain
        } else {
            ActiveBackend::File
        }
    }

    fn write_active_oauth(&self, credentials: &str) -> Result<ActiveBackend> {
        if self.use_keychain() {
            match self.kc(|k| k.set_password(ACTIVE_OAUTH_SERVICE, k.account(), credentials)) {
                Ok(()) => {
                    self.refresh_present_credentials_file(credentials);
                    return Ok(ActiveBackend::Keychain);
                }
                Err(e) => self.warn(format!("Keychain write failed, falling back to file: {e}")),
            }
        }
        self.write_active_credentials_file(credentials)
            .map_err(|e| write_error(format!("Failed to write credentials: {e}")))?;
        self.delete_active_oauth_item();
        self.pin_file_mode();
        Ok(ActiveBackend::File)
    }

    /// A running Claude Code reloads its token when this file's mtime moves;
    /// it is rewritten only when present so Keychain-only users stay fileless.
    fn refresh_present_credentials_file(&self, credentials: &str) {
        if !self.paths.credentials_path().exists() {
            return;
        }
        if let Err(e) = self.write_active_credentials_file(credentials) {
            self.warn(format!(
                "Could not refresh .credentials.json after Keychain write ({e}); a running session may not hot-reload until restart"
            ));
        }
    }

    fn write_active_credentials_file(&self, credentials: &str) -> std::io::Result<()> {
        let dir = self.paths.claude_config_home();
        fsutil::atomic_write_private(&dir, &self.paths.credentials_path(), credentials.as_bytes())
    }

    fn delete_active_oauth_item(&self) {
        let _ = self
            .keychain
            .delete_password(ACTIVE_OAUTH_SERVICE, self.keychain.account());
    }

    /// After a write fell back to the file, a stale Keychain item may
    /// survive; never re-probe onto it for the rest of the process.
    fn pin_file_mode(&self) {
        let mut cap = self.capability();
        cap.usable = Some(false);
        cap.reprobe_at = None;
    }

    fn write_managed_key(&self, api_key: &str) -> Result<ActiveBackend> {
        let mut in_keychain = false;
        if self.use_keychain() {
            match self.kc(|k| k.set_password(MANAGED_KEY_SERVICE, k.account(), api_key)) {
                Ok(()) => in_keychain = true,
                Err(e) => self.warn(format!(
                    "Managed-key Keychain write failed, falling back to config: {e}"
                )),
            }
        }
        let approved = approved_form(api_key);
        self.update_global_config(|config| {
            let responses = config
                .entry("customApiKeyResponses")
                .or_insert_with(|| Value::Object(Map::new()));
            if !responses.is_object() {
                *responses = Value::Object(Map::new());
            }
            let responses = responses.as_object_mut().expect("object");
            let list = responses
                .entry("approved")
                .or_insert_with(|| Value::Array(Vec::new()));
            if !list.is_array() {
                *list = Value::Array(Vec::new());
            }
            let list = list.as_array_mut().expect("array");
            if !list.iter().any(|v| v.as_str() == Some(&approved)) {
                list.push(approved.clone().into());
            }
            responses
                .entry("rejected")
                .or_insert_with(|| Value::Array(Vec::new()));
            if in_keychain {
                config.shift_remove("primaryApiKey");
            } else {
                config.insert("primaryApiKey".into(), api_key.into());
            }
        })?;
        self.delete_active_oauth_item();
        let credentials_file = self.paths.credentials_path();
        if credentials_file.exists()
            && let Err(e) = std::fs::remove_file(&credentials_file)
        {
            self.warn(format!("Failed to remove credentials file: {e}"));
        }
        if in_keychain {
            Ok(ActiveBackend::Keychain)
        } else {
            self.pin_file_mode();
            Ok(ActiveBackend::File)
        }
    }

    /// Claude Code's `removeApiKey`: drop the Keychain key and `primaryApiKey`,
    /// keep `approved`. Best-effort.
    fn clear_managed_key(&self) {
        let _ = self
            .keychain
            .delete_password(MANAGED_KEY_SERVICE, self.keychain.account());
        let path = self.paths.global_config_path();
        let config = super::read_json_object(&path);
        if config.is_none() && path.exists() {
            self.warn(
                "Could not clear primaryApiKey: the global config exists but could not be read (unreadable, not absent) — leaving it in place rather than overwriting it unread".into(),
            );
            return;
        }
        if config.is_some_and(|c| c.get("primaryApiKey").is_some_and(|v| !v.is_null()))
            && let Err(e) = self.update_global_config(|c| {
                c.shift_remove("primaryApiKey");
            })
        {
            self.warn(format!("Failed to clear primaryApiKey: {e}"));
        }
    }

    /// Rewrites `~/.claude.json` with only `mutate`'s keys changed; refuses
    /// to replace a file that exists but cannot be read.
    fn update_global_config(&self, mutate: impl FnOnce(&mut Map<String, Value>)) -> Result<()> {
        let path = self.paths.global_config_path();
        let mut config = match super::read_json_object(&path) {
            Some(config) => config,
            None if path.exists() => {
                return Err(write_error(format!(
                    "{} exists but could not be read — refusing to overwrite it. Move or repair the file, then retry.",
                    path.display()
                )));
            }
            None => Map::new(),
        };
        mutate(&mut config);
        let dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let text = pyjson::to_string_pretty(&Value::Object(config));
        std::fs::create_dir_all(&dir)
            .and_then(|()| fsutil::write_private_in(&dir, "", &path, text.as_bytes()))
            .map_err(|e| write_error(format!("Failed to write managed API key: {e}")))
    }
}

/// The last 20 characters, as Claude Code records an approved key.
fn approved_form(api_key: &str) -> String {
    let chars: Vec<char> = api_key.trim().chars().collect();
    chars[chars.len().saturating_sub(20)..].iter().collect()
}

fn write_error(message: String) -> Error {
    Error::new(ErrorKind::CredentialWriteError, message)
}
