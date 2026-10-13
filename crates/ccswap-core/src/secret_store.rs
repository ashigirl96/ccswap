use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::env::{Environment, passwd_user_name};
use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::paths::Paths;

mod active_write;
pub use active_write::ActiveBackend;

pub const ACTIVE_OAUTH_SERVICE: &str = "Claude Code-credentials";
pub const MANAGED_KEY_SERVICE: &str = "Claude Code";
pub const BACKUP_SERVICE: &str = "claude-swap";

const NOT_FOUND_RC: i32 = 44;
const SECURITY_TIMEOUT: Duration = Duration::from_secs(5);
/// `security -i` reads one line into a 4096-byte buffer; longer commands go to argv.
const SECURITY_STDIN_LINE_LIMIT: usize = 4096 - 64;
const ACTIVE_READ_ATTEMPTS: usize = 2;
const ACTIVE_READ_RETRY_DELAY: Duration = Duration::from_millis(300);
const KEYCHAIN_RECHECK_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainError(pub String);

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
    ) -> std::result::Result<Option<String>, KeychainError> {
        let out = self.run(
            &["find-generic-password", "-a", account, "-w", "-s", service],
            None,
        )?;
        match out.code {
            Some(0) => Ok(Some(
                out.stdout
                    .strip_suffix('\n')
                    .unwrap_or(&out.stdout)
                    .to_string(),
            )),
            Some(NOT_FOUND_RC) => Ok(None),
            code => Err(failed("find-generic-password", code, &out.stderr)),
        }
    }

    /// Create or update (`-U`). The secret goes hex-encoded through
    /// `security -i`'s stdin; only a command line too long for its buffer
    /// falls back to argv.
    pub fn set_password(
        &self,
        service: &str,
        account: &str,
        password: &str,
    ) -> std::result::Result<(), KeychainError> {
        let hex: String = password.bytes().map(|b| format!("{b:02x}")).collect();
        let command = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            quote(account),
            quote(service)
        );
        let out = if command.len() <= SECURITY_STDIN_LINE_LIMIT {
            self.run(&["-i"], Some(&command))?
        } else {
            self.run(
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    account,
                    "-s",
                    service,
                    "-X",
                    &hex,
                ],
                None,
            )?
        };
        match out.code {
            Some(0) => Ok(()),
            code => Err(failed("add-generic-password", code, &out.stderr)),
        }
    }

    /// Absent already (exit 44) counts as success.
    pub fn delete_password(
        &self,
        service: &str,
        account: &str,
    ) -> std::result::Result<(), KeychainError> {
        self.delete_if_present(service, account).map(|_| ())
    }

    /// Like [`Keychain::delete_password`], telling whether an item was there.
    pub fn delete_if_present(
        &self,
        service: &str,
        account: &str,
    ) -> std::result::Result<bool, KeychainError> {
        let out = self.run(
            &["delete-generic-password", "-a", account, "-s", service],
            None,
        )?;
        match out.code {
            Some(0) => Ok(true),
            Some(NOT_FOUND_RC) => Ok(false),
            code => Err(failed("delete-generic-password", code, &out.stderr)),
        }
    }

    /// Attribute-only lookup: never decrypts, so it cannot prompt. Any
    /// failure reads as "not there".
    pub fn item_exists(&self, service: &str, account: &str) -> bool {
        self.run(
            &["find-generic-password", "-a", account, "-s", service],
            None,
        )
        .is_ok_and(|out| out.code == Some(0))
    }

    fn run(
        &self,
        args: &[&str],
        stdin: Option<&str>,
    ) -> std::result::Result<SecurityOutput, KeychainError> {
        let mut child = Command::new(&self.security)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                KeychainError(format!("could not run {}: {e}", self.security.display()))
            })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(input.as_bytes());
        }
        let op = if args[0] == "-i" {
            "add-generic-password"
        } else {
            args[0]
        };
        let deadline = Instant::now() + SECURITY_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|e| KeychainError(e.to_string()))? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(KeychainError(format!("security {op} timed out after 5.0s")));
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

fn failed(op: &str, code: Option<i32>, stderr: &str) -> KeychainError {
    KeychainError(format!(
        "security {op} failed (rc={}): {}",
        code.map_or("signal".into(), |c| c.to_string()),
        stderr.trim()
    ))
}

/// `security -i` tokenises like a shell: wrap in quotes, escape `\` and `"`.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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

/// A slot backup read that tells "absent" (`value` empty, not `unreadable`)
/// from "there but could not be read".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRead {
    pub value: String,
    pub unreadable: bool,
}

/// Per-process Keychain usability, learned from real `security` calls. Once
/// an op fails, backup writes route to files for the rest of the command;
/// a long-running process re-probes after a cooldown.
#[derive(Debug, Default)]
struct Capability {
    usable: Option<bool>,
    reprobe_at: Option<Instant>,
    op_failed: bool,
}

/// Claude Code's active credential and claude-swap's per-slot backups,
/// across the Keychain and the file backend.
pub struct SecretStore {
    keychain: Keychain,
    paths: Paths,
    securestorage_dir: Option<String>,
    capability: RefCell<Capability>,
    warnings: RefCell<Vec<String>>,
}

impl SecretStore {
    pub fn new(keychain: Keychain, paths: Paths, env: &Environment) -> Self {
        SecretStore {
            keychain,
            paths,
            securestorage_dir: env
                .get("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .map(|v| v.to_string_lossy().into_owned()),
            capability: RefCell::default(),
            warnings: RefCell::default(),
        }
    }

    pub fn keychain(&self) -> &Keychain {
        &self.keychain
    }

    /// A slot backup from the Keychain only (no `.enc`), `""` when absent.
    pub fn read_backup_keychain(
        &self,
        num: &str,
        email: &str,
    ) -> std::result::Result<String, KeychainError> {
        let account = Self::backup_account_name(num, email);
        self.kc(|k| k.get_password(BACKUP_SERVICE, &account))
            .map(Option::unwrap_or_default)
    }

    pub fn write_backup_keychain(
        &self,
        num: &str,
        email: &str,
        credentials: &str,
    ) -> std::result::Result<(), KeychainError> {
        let account = Self::backup_account_name(num, email);
        self.kc(|k| k.set_password(BACKUP_SERVICE, &account, credentials))
    }

    pub fn delete_backup_keychain_quiet(&self, num: &str, email: &str) {
        let account = Self::backup_account_name(num, email);
        if let Err(e) = self.kc(|k| k.delete_password(BACKUP_SERVICE, &account)) {
            self.warn(format!("Failed to delete credentials from Keychain: {e}"));
        }
    }

    /// Diagnostics upstream sends to its log file, oldest first.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.warnings.borrow_mut())
    }

    fn warn(&self, message: String) {
        self.warnings.borrow_mut().push(message);
    }

    /// Runs a Keychain op and records what it says about Keychain usability.
    fn kc<T>(
        &self,
        op: impl FnOnce(&Keychain) -> std::result::Result<T, KeychainError>,
    ) -> std::result::Result<T, KeychainError> {
        let result = op(&self.keychain);
        let mut cap = self.capability.borrow_mut();
        match &result {
            Ok(_) => {
                cap.op_failed = false;
                if cap.usable.is_none() {
                    cap.usable = Some(true);
                }
            }
            Err(_) => {
                cap.op_failed = true;
                cap.usable = Some(false);
                cap.reprobe_at = Some(Instant::now() + KEYCHAIN_RECHECK_COOLDOWN);
            }
        }
        result
    }

    /// Whether backup writes should target the Keychain right now.
    pub fn use_keychain(&self) -> bool {
        let mut cap = self.capability.borrow_mut();
        if cap.usable == Some(false) && cap.reprobe_at.is_some_and(|t| Instant::now() >= t) {
            cap.usable = None;
            cap.reprobe_at = None;
        }
        cap.usable != Some(false)
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

    fn read_active_oauth_keychain(&self) -> std::result::Result<Option<String>, KeychainError> {
        for service in self.active_oauth_services() {
            if let Some(value) = self.read_with_retry(&service)?.filter(|v| !v.is_empty()) {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn read_with_retry(&self, service: &str) -> std::result::Result<Option<String>, KeychainError> {
        let mut last = None;
        for attempt in 0..ACTIVE_READ_ATTEMPTS {
            match self.kc(|k| k.get_password(service, k.account())) {
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
            && let Ok(Some(value)) = self.kc(|k| k.get_password(MANAGED_KEY_SERVICE, k.account()))
            && !value.is_empty()
        {
            return value;
        }
        self.primary_api_key()
    }

    fn primary_api_key(&self) -> String {
        read_json_object(&self.paths.global_config_path())
            .and_then(|cfg| {
                cfg.get("primaryApiKey")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    /// The credential of the profile this environment points Claude Code at,
    /// for capture into a slot (`_read_capture_credentials`). `Ok(None)` is
    /// "could not be read", `Ok(Some(""))` is "logged out". An unreadable
    /// Keychain is an error rather than a silent fall back to a plaintext
    /// copy that may be a superseded generation.
    pub fn read_capture(&self) -> Result<Option<String>> {
        if let Some(secure) = &self.securestorage_dir {
            let (dir, service) = if secure.is_empty() {
                (
                    self.paths
                        .default_claude_config_home()
                        .display()
                        .to_string(),
                    Some(ACTIVE_OAUTH_SERVICE),
                )
            } else {
                (secure.clone(), None)
            };
            let creds = self.read_config_dir_credentials(&dir, service)?;
            return Ok(Some(creds.unwrap_or_default()));
        }
        let Some(config_dir) = self.paths.config_dir_env() else {
            return self.refuse_degraded_capture();
        };
        if let Some(creds) = self
            .read_config_dir_credentials(config_dir, None)?
            .filter(|c| !c.is_empty())
        {
            return Ok(Some(creds));
        }
        if self.paths.active_profile_is_default() {
            return self.refuse_degraded_capture();
        }
        Ok(Some(self.primary_api_key()))
    }

    fn refuse_degraded_capture(&self) -> Result<Option<String>> {
        let active = self.read_active();
        if active.degraded {
            return Err(Error::new(
                ErrorKind::CredentialReadError,
                "The macOS Keychain is unreadable right now (locked or no GUI session), so the only readable credential is a plaintext fallback that may be a superseded generation — capturing it would file a spent refresh token against this slot. Retry from a GUI terminal.",
            ));
        }
        Ok(active.value)
    }

    /// `session.read_config_dir_credentials` with `strict_keychain=True`.
    fn read_config_dir_credentials(
        &self,
        config_dir: &str,
        service: Option<&str>,
    ) -> Result<Option<String>> {
        let directory = Path::new(config_dir);
        if !directory.is_dir() {
            return Ok(None);
        }
        let service = service
            .map(str::to_string)
            .unwrap_or_else(|| profile_service_name(config_dir));
        let mut last = None;
        for attempt in 0..ACTIVE_READ_ATTEMPTS {
            match self
                .keychain
                .get_password(&service, self.keychain.account())
            {
                Ok(Some(creds)) if !creds.is_empty() => return Ok(Some(creds)),
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => last = Some(e),
            }
            if attempt + 1 < ACTIVE_READ_ATTEMPTS {
                std::thread::sleep(ACTIVE_READ_RETRY_DELAY);
            }
        }
        if let Some(e) = last {
            return Err(Error::new(
                ErrorKind::CredentialReadError,
                format!(
                    "Keychain entry for profile {config_dir} is unreadable (locked or busy) — unlock the keychain and retry: {e}"
                ),
            ));
        }
        Ok(std::fs::read_to_string(directory.join(".credentials.json")).ok())
    }

    pub fn backup_account_name(num: &str, email: &str) -> String {
        format!("account-{num}-{email}")
    }

    pub fn backup_enc_path(&self, num: &str, email: &str) -> PathBuf {
        self.paths
            .credentials_dir()
            .join(format!(".creds-{num}-{email}.enc"))
    }

    pub fn previous_enc_path(&self, num: &str, email: &str) -> PathBuf {
        self.paths
            .credentials_dir()
            .join(format!(".creds-{num}-{email}.enc.prev"))
    }

    /// A slot's backup. A non-empty, valid `.enc` wins over the Keychain copy
    /// (it was written while the Keychain was unusable and is newer); an
    /// absent, empty or corrupt one falls through to the Keychain.
    pub fn read_backup(&self, num: &str, email: &str) -> BackupRead {
        let mut unreadable = false;
        let enc = self.backup_enc_path(num, email);
        match std::fs::metadata(&enc) {
            Ok(_) => match std::fs::read_to_string(&enc) {
                Ok(encoded) => match decode_b64(&encoded) {
                    Some(decoded) if !decoded.is_empty() => {
                        return BackupRead {
                            value: decoded,
                            unreadable: false,
                        };
                    }
                    Some(_) => {}
                    None => self.warn("Failed to read credentials file: invalid base64".into()),
                },
                Err(e) => {
                    unreadable = true;
                    self.warn(format!("Failed to read credentials file: {e}"));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                unreadable = true;
                self.warn(format!("Failed to read credentials file: {e}"));
            }
        }
        let account = Self::backup_account_name(num, email);
        match self.kc(|k| k.get_password(BACKUP_SERVICE, &account)) {
            Ok(value) => {
                let value = value.unwrap_or_default();
                BackupRead {
                    unreadable: unreadable && value.is_empty(),
                    value,
                }
            }
            Err(e) => {
                self.warn(format!("Failed to read credentials from Keychain: {e}"));
                BackupRead {
                    value: String::new(),
                    unreadable: true,
                }
            }
        }
    }

    /// The retained previous generation; `.enc.prev` wins like the backup.
    pub fn read_previous_backup(&self, num: &str, email: &str) -> String {
        if let Ok(encoded) = std::fs::read_to_string(self.previous_enc_path(num, email))
            && let Some(decoded) = decode_b64(&encoded).filter(|d| !d.is_empty())
        {
            return decoded;
        }
        let account = format!("{}.prev", Self::backup_account_name(num, email));
        self.kc(|k| k.get_password(BACKUP_SERVICE, &account))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Store a slot backup: the Keychain while it is usable (then the `.enc`
    /// is removed so it cannot shadow the new item), else an atomic `.enc`
    /// (then the stale Keychain item is removed best-effort). The replaced
    /// generation is kept as `.prev` first.
    pub fn write_backup(&self, num: &str, email: &str, credentials: &str) -> Result<()> {
        self.retain_previous_backup(num, email, credentials);
        let account = Self::backup_account_name(num, email);
        if self.use_keychain() {
            match self.kc(|k| k.set_password(BACKUP_SERVICE, &account, credentials)) {
                Ok(()) => return self.reconcile_enc_after_keychain_write(num, email, credentials),
                Err(e) => self.warn(format!(
                    "Keychain backup write failed, falling back to file: {e}"
                )),
            }
        }
        self.write_backup_enc(&self.backup_enc_path(num, email), credentials)?;
        if let Err(e) = self.kc(|k| k.delete_password(BACKUP_SERVICE, &account)) {
            self.warn(format!("Failed to delete credentials from Keychain: {e}"));
        }
        Ok(())
    }

    fn retain_previous_backup(&self, num: &str, email: &str, incoming: &str) {
        let current = self.read_backup(num, email);
        if current.unreadable {
            self.warn(format!(
                "Could not retain previous credential generation for account {num}: the current backup exists but could not be read (not absent) — no .prev recovery copy will exist for this write"
            ));
            return;
        }
        if current.value.is_empty() || current.value == incoming {
            return;
        }
        let result = if self.use_keychain() {
            let account = format!("{}.prev", Self::backup_account_name(num, email));
            self.kc(|k| k.set_password(BACKUP_SERVICE, &account, &current.value))
                .map_err(|e| e.0)
        } else {
            self.write_backup_enc(&self.previous_enc_path(num, email), &current.value)
                .map_err(|e| e.message)
        };
        if let Err(e) = result {
            self.warn(format!(
                "Failed to retain previous credential generation for account {num}: {e}"
            ));
        }
    }

    fn reconcile_enc_after_keychain_write(
        &self,
        num: &str,
        email: &str,
        credentials: &str,
    ) -> Result<()> {
        let enc = self.backup_enc_path(num, email);
        if !enc.exists() {
            return Ok(());
        }
        match std::fs::remove_file(&enc) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.warn(format!(
                    "Could not delete .enc after Keychain backup write ({e}); rewriting it with the fresh credentials to keep both consistent"
                ));
                self.write_backup_enc(&enc, credentials)
            }
        }
    }

    fn write_backup_enc(&self, target: &Path, credentials: &str) -> Result<()> {
        let encoded = BASE64.encode(credentials.as_bytes());
        fsutil::atomic_write_private(&self.paths.credentials_dir(), target, encoded.as_bytes())
            .map_err(|e| {
                Error::new(
                    ErrorKind::CredentialWriteError,
                    format!("Failed to write credentials file {}: {e}", target.display()),
                )
            })
    }

    /// Best-effort removal of a slot's backups in both backends, including
    /// the legacy `account-None-<email>` key and the `.prev` generation.
    pub fn delete_backup(&self, num: &str, email: &str) {
        let mut nums = vec![num];
        if num != "None" {
            nums.push("None");
        }
        for n in nums {
            let enc = self.backup_enc_path(n, email);
            if enc.exists()
                && let Err(e) = std::fs::remove_file(&enc)
            {
                self.warn(format!("Failed to delete credentials file: {e}"));
            }
            let account = Self::backup_account_name(n, email);
            if let Err(e) = self.kc(|k| k.delete_password(BACKUP_SERVICE, &account)) {
                self.warn(format!("Failed to delete credentials from Keychain: {e}"));
            }
            self.delete_previous_backup(n, email);
        }
    }

    pub fn delete_previous_backup(&self, num: &str, email: &str) {
        let prev = self.previous_enc_path(num, email);
        if prev.exists()
            && let Err(e) = std::fs::remove_file(&prev)
        {
            self.warn(format!("Failed to delete .prev file: {e}"));
        }
        let account = format!("{}.prev", Self::backup_account_name(num, email));
        if let Err(e) = self.kc(|k| k.delete_password(BACKUP_SERVICE, &account)) {
            self.warn(format!("Failed to delete .prev from Keychain: {e}"));
        }
    }
}

/// Python's `b64decode(validate=True)` then UTF-8; `None` when corrupt.
fn decode_b64(encoded: &str) -> Option<String> {
    let bytes = BASE64.decode(encoded.trim()).ok()?;
    String::from_utf8(bytes).ok()
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

    #[test]
    fn quote_escapes_backslash_then_double_quote() {
        assert_eq!(quote(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(quote("Claude Code"), "\"Claude Code\"");
    }

    #[test]
    fn corrupt_base64_is_not_a_backup() {
        assert_eq!(decode_b64("!!!!"), None);
        assert_eq!(decode_b64("eyJhIjoxfQ==\n").as_deref(), Some("{\"a\":1}"));
    }
}
