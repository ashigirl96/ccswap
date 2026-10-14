//! Session mode (`run`): Claude Code launched as a stored account in one
//! terminal, with `CLAUDE_CONFIG_DIR` pointing at a persistent per-account
//! profile under `<backup>/sessions/<num>-<slug>/`. The profile is seeded with
//! a plaintext `.credentials.json`, which Claude migrates into its own hashed
//! Keychain item on first write.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::accounts::{Accounts, Console, ResolvedAccount};
use crate::anthropic_client::{AnthropicClient, credential_fingerprint};
use crate::consume_gate::ConsumeGate;
use crate::env::Environment;
use crate::error::{Error, ErrorKind, Result};
use crate::locks::StoreLock;
use crate::paths::Paths;
use crate::presentation::Style;
use crate::pyjson;
use crate::secret_store::{SecretStore, profile_service_name};
use crate::session_profile::{self, profile_is_quiescent};
use crate::store::{self, JsonObject};

mod sharing;

/// Variables that make Claude bypass account OAuth: dropped from the probe
/// and scrubbed from the session launch, since `run N` asks for account N.
pub const AUTH_OVERRIDE_ENV_VARS: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
];

const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// Bootstrap holds the store lock across probes, so it waits longer than
/// the switch paths.
const BOOTSTRAP_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOptions {
    pub share: bool,
    pub share_history: bool,
    pub require_session: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            share: true,
            share_history: false,
            require_session: false,
        }
    }
}

/// What to exec: `program` is also argv[0], `env` the complete environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, OsString)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Valid,
    Invalid,
    /// The probe ran and did not answer.
    Unknown,
    /// `claude` could not be spawned.
    Unreachable,
}

pub struct Sessions<'a> {
    pub paths: &'a Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
    pub style: &'a Style,
}

struct Target {
    num: String,
    email: String,
    org: String,
    dir: PathBuf,
}

impl Sessions<'_> {
    /// Prepares the launch of `identifier`'s account; the caller execs it.
    pub fn run(
        &self,
        identifier: &str,
        claude_args: &[String],
        options: RunOptions,
        console: &mut dyn Console,
    ) -> Result<Launch> {
        let claude = self.claude_bin()?;
        let ResolvedAccount {
            number: num,
            email,
            organization_uuid: org,
        } = self.accounts().resolve_account(identifier)?;
        self.ensure_not_api_key(&num, &email)?;

        if let Some(preset) = self.env.non_empty("CLAUDE_CONFIG_DIR") {
            console.line(&self.style.yellowed(&format!(
                "CLAUDE_CONFIG_DIR is already set ({preset}); overriding it for this launch."
            )));
        } else if store::current_identity(self.paths)
            .is_some_and(|i| i.email == email && i.organization_uuid == org)
        {
            if options.require_session {
                return Err(session_error(format!(
                    "Account-{num} ({email}) is the active default login, so this launch would run plain claude on the default login rather than in a session profile (a second copy of the active credential would drift). Switch the default login to another account first, or run `claude` directly."
                )));
            }
            console.line(&self.style.dimmed(&format!(
                "Account-{num} ({email}) is already the active default login — launching claude directly."
            )));
            return Ok(self.launch(claude, claude_args, self.full_env()));
        }

        let scrubbed: Vec<&str> = AUTH_OVERRIDE_ENV_VARS
            .into_iter()
            .filter(|v| self.env.non_empty(v).is_some())
            .collect();
        if !scrubbed.is_empty() {
            console.line(&self.style.yellowed(&format!(
                "Ignoring {} for this session — it would override the selected account inside Claude Code.",
                scrubbed.join(", ")
            )));
        }

        let target = Target {
            dir: session_profile::session_dir_for(self.paths, &num, &email),
            num,
            email,
            org,
        };
        self.setup_session(&target, options, console)?;

        console.line(&format!(
            "{} Account-{} ({}) {}",
            self.style.accent("Launching"),
            target.num,
            target.email,
            self.style.muted("[session mode]")
        ));
        let mut env = self.scrubbed_env();
        env.push(("CLAUDE_CONFIG_DIR".into(), target.dir.clone().into()));
        Ok(self.launch(claude, claude_args, env))
    }

    /// Plain `claude` on the default login with the untouched environment.
    pub fn exec_default(&self, claude_args: &[String]) -> Result<Launch> {
        let claude = self.claude_bin()?;
        Ok(self.launch(claude, claude_args, self.full_env()))
    }

    fn launch(&self, program: PathBuf, args: &[String], env: Vec<(String, OsString)>) -> Launch {
        Launch {
            program,
            args: args.to_vec(),
            env,
        }
    }

    fn claude_bin(&self) -> Result<PathBuf> {
        which(self.env, "claude").ok_or_else(|| {
            session_error("'claude' was not found on PATH. Install Claude Code first.".into())
        })
    }

    fn accounts(&self) -> Accounts<'_> {
        Accounts {
            paths: self.paths,
            env: self.env,
            secrets: self.secrets,
            client: self.client,
            style: self.style,
        }
    }

    fn ensure_not_api_key(&self, num: &str, email: &str) -> Result<()> {
        let data = store::read_sequence(self.paths)?.unwrap_or_default();
        let kind = store::accounts(&data)
            .and_then(|a| a.get(num))
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str);
        if kind == Some("api_key") {
            return Err(session_error(format!(
                "Account-{num} ({email}) is an API-key account; 'ccswap run' (session mode) does not support API-key accounts yet. Use 'ccswap --switch-to' to make it your default login instead."
            )));
        }
        Ok(())
    }

    fn full_env(&self) -> Vec<(String, OsString)> {
        self.env
            .vars()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn scrubbed_env(&self) -> Vec<(String, OsString)> {
        self.env
            .vars()
            .filter(|(k, _)| !AUTH_OVERRIDE_ENV_VARS.contains(k) && *k != "CLAUDE_CONFIG_DIR")
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// The stale marker is honoured only while nothing runs against the profile.
    fn setup_session(
        &self,
        target: &Target,
        options: RunOptions,
        console: &mut dyn Console,
    ) -> Result<()> {
        let dir = &target.dir;
        let stale = session_profile::is_stale(dir) && profile_is_quiescent(dir);
        if !stale && self.is_session_valid(target) {
            self.sync_sharing(dir, options, console);
            return Ok(());
        }

        self.refresh_before_seeding(target, console)?;

        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), BOOTSTRAP_LOCK_TIMEOUT)?;
        if session_profile::is_stale(dir) && profile_is_quiescent(dir) {
            session_profile::invalidate_slot_credentials(
                self.secrets.keychain(),
                self.paths,
                &target.num,
                &target.email,
            );
            session_profile::clear_stale(dir);
        }
        if self.is_session_valid(target) {
            // A valid profile may predate the backup's current generation;
            // re-seed it, but never beneath a running claude.
            if !self.profile_matches_backup(target) && profile_is_quiescent(dir) {
                self.bootstrap(target)?;
            }
            self.sync_sharing(dir, options, console);
            return Ok(());
        }

        self.bootstrap(target)?;
        self.sync_sharing(dir, options, console);

        let mut verdict = self.session_validity(target);
        if verdict == Verdict::Unknown && self.artifacts_say_usable(target) {
            verdict = Verdict::Valid;
        }
        let (num, email) = (&target.num, &target.email);
        match verdict {
            Verdict::Valid => Ok(()),
            Verdict::Unknown | Verdict::Unreachable => Err(session_error(format!(
                "Session profile for Account-{num} ({email}) could not be verified: `claude auth status` did not run or did not answer. The profile is left in place — check that `claude` is on PATH, then retry."
            ))),
            Verdict::Invalid => {
                session_profile::delete_profile(self.secrets.keychain(), dir);
                Err(session_error(format!(
                    "Session profile for Account-{num} ({email}) failed validation. Log in with that account and re-add it: ccswap --add-account --slot {num}"
                )))
            }
        }
    }

    /// One refresh so the profile starts with a fresh access token, before
    /// the bootstrap lock (the gate POSTs, and takes the store lock itself).
    /// A plain failure is not fatal; a refresh whose successor could not be
    /// stored is, since bootstrapping would seed the spent generation.
    fn refresh_before_seeding(&self, target: &Target, console: &mut dyn Console) -> Result<()> {
        let num = &target.num;
        let creds = self.secrets.read_backup(num, &target.email).value;
        if creds.is_empty() || !has_refresh_token(&creds) {
            return Ok(());
        }
        let outcome = ConsumeGate {
            paths: self.paths,
            env: self.env,
            secrets: self.secrets,
            client: self.client,
        }
        .consume_backup_grant(num, &target.email, &creds);
        if outcome.error.is_some() && outcome.credentials.is_some() {
            return Err(session_error(if outcome.stashed {
                format!(
                    "Account-{num}'s refreshed credential could not be stored, so the backup still holds a spent grant. The successor is stashed — please retry, and the next run adopts it automatically."
                )
            } else {
                format!(
                    "Account-{num}'s refreshed credential could neither be stored nor stashed, so the backup holds a spent grant and the successor is gone. Fix the storage failure first; retrying before that spends nothing but earns a strike. If the slot strikes, log in again and re-add it: ccswap --add-account --slot {num}"
                )
            }));
        }
        if outcome.error.is_some() {
            console.line(&self.style.yellowed(&format!(
                "Could not refresh the token for Account-{num}; continuing with the stored credentials."
            )));
        }
        Ok(())
    }

    /// Seeds the profile from the backup. The caller holds the store lock.
    fn bootstrap(&self, target: &Target) -> Result<()> {
        let (dir, num) = (&target.dir, &target.num);
        // Claude reads the Keychain before the file: a leftover hashed item
        // would shadow the fresh seed.
        session_profile::delete_keychain_entry(self.secrets.keychain(), dir);

        let backup = self.secrets.read_backup(num, &target.email);
        if backup.value.is_empty() {
            if backup.unreadable {
                return Err(session_error(format!(
                    "Account-{num}'s backup is in the macOS Keychain but it is unreadable right now (locked or no GUI session). Retry from a GUI terminal; do not re-add."
                )));
            }
            return Err(session_error(format!(
                "Account-{num} has no stored credentials. Re-add with: ccswap --add-account --slot {num}"
            )));
        }

        let config_text = store::read_account_config(self.paths, num, &target.email);
        let config: JsonObject = serde_json::from_str::<Value>(&config_text)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let oauth_account = config
            .get("oauthAccount")
            .filter(|v| store::is_truthy(Some(v)))
            .cloned()
            .ok_or_else(|| {
                session_error(format!(
                    "Account-{num} has no stored config backup. Re-add with: ccswap --add-account --slot {num}"
                ))
            })?;

        let io =
            |e: std::io::Error| session_error(format!("Could not seed {}: {e}", dir.display()));
        std::fs::create_dir_all(dir).map_err(io)?;
        set_mode(dir, 0o700).map_err(io)?;

        let creds_path = dir.join(".credentials.json");
        std::fs::write(&creds_path, &backup.value).map_err(io)?;
        set_mode(&creds_path, 0o600).map_err(io)?;

        // Merged, so a re-bootstrap keeps the profile's own projects and
        // history. Claude shows onboarding without `theme`.
        let config_path = dir.join(".claude.json");
        let mut existing: JsonObject = std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        existing.insert("oauthAccount".into(), oauth_account);
        existing.insert("hasCompletedOnboarding".into(), Value::Bool(true));
        if !existing.contains_key("theme") {
            let theme = config
                .get("theme")
                .filter(|v| store::is_truthy(Some(v)))
                .cloned()
                .unwrap_or_else(|| Value::String("dark".into()));
            existing.insert("theme".into(), theme);
        }
        std::fs::write(
            &config_path,
            pyjson::to_string_pretty(&Value::Object(existing)),
        )
        .map_err(io)?;
        set_mode(&config_path, 0o600).map_err(io)?;
        crate::log::info(format!(
            "Bootstrapped session profile for account {num} at {}",
            dir.display()
        ));
        Ok(())
    }

    /// Same credential generation as the backup; unknowable reads as a match
    /// so a read error never throws a working profile away.
    fn profile_matches_backup(&self, target: &Target) -> bool {
        let profile = session_profile::read_credentials(self.secrets.keychain(), &target.dir);
        let backup = self.secrets.read_backup(&target.num, &target.email).value;
        match profile {
            Some(profile) if !profile.is_empty() && !backup.is_empty() => {
                credential_fingerprint(&profile) == credential_fingerprint(&backup)
            }
            _ => true,
        }
    }

    /// Reuse view of the probe: an unanswered probe falls back to what the
    /// profile's files say; an unrunnable `claude` never reuses.
    fn is_session_valid(&self, target: &Target) -> bool {
        match self.session_validity(target) {
            Verdict::Valid => true,
            Verdict::Unknown => self.artifacts_say_usable(target),
            Verdict::Invalid | Verdict::Unreachable => false,
        }
    }

    fn artifacts_say_usable(&self, target: &Target) -> bool {
        self.may_have_credential_material(&target.dir)
            && !session_profile::identity_drifted(&target.dir, &target.email, &target.org)
    }

    /// False only when every store is positively empty; an unreadable
    /// Keychain leans present.
    fn may_have_credential_material(&self, dir: &Path) -> bool {
        if std::fs::read_to_string(dir.join(".credentials.json")).is_ok_and(|t| !t.is_empty()) {
            return true;
        }
        let keychain = self.secrets.keychain();
        !matches!(
            keychain.get_password(
                &profile_service_name(&dir.display().to_string()),
                keychain.account()
            ),
            Ok(None)
        )
    }

    /// `claude auth status --json` against the profile. A local check: a
    /// revoked but unexpired token still passes.
    fn session_validity(&self, target: &Target) -> Verdict {
        if !target.dir.is_dir() {
            return Verdict::Invalid;
        }
        let claude = which(self.env, "claude").unwrap_or_else(|| PathBuf::from("claude"));
        let mut env = self.scrubbed_env();
        env.push(("CLAUDE_CONFIG_DIR".into(), target.dir.clone().into()));
        let child = Command::new(&claude)
            .args(["auth", "status", "--json"])
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return Verdict::Unreachable;
        };
        let mut stdout = child.stdout.take().expect("piped stdout");
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let deadline = Instant::now() + AUTH_STATUS_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Verdict::Unknown;
                }
            }
        };
        let output = reader.join().unwrap_or_default();
        if !status.success() {
            return Verdict::Invalid;
        }
        let Some(report) = String::from_utf8(output)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.as_object().cloned())
        else {
            return Verdict::Unknown;
        };
        judge_auth_status(&report, &target.email, &target.org)
    }
}

fn judge_auth_status(report: &Map<String, Value>, email: &str, org: &str) -> Verdict {
    if report.get("loggedIn") != Some(&Value::Bool(true))
        || report.get("authMethod").and_then(Value::as_str) != Some("claude.ai")
        || report.get("email").and_then(Value::as_str) != Some(email)
    {
        return Verdict::Invalid;
    }
    let status_org = report.get("orgId");
    if store::is_truthy(status_org)
        && !org.is_empty()
        && status_org.and_then(Value::as_str) != Some(org)
    {
        return Verdict::Invalid;
    }
    Verdict::Valid
}

/// Setup-token credentials have no refresh token by design; an unknown
/// shape lets the refresh attempt decide.
fn has_refresh_token(creds: &str) -> bool {
    match serde_json::from_str::<Value>(creds) {
        Ok(Value::Object(data)) => match data.get("claudeAiOauth") {
            None => false,
            Some(Value::Object(oauth)) => {
                oauth.get("refreshToken").is_some_and(crate::usage::truthy)
            }
            Some(_) => true,
        },
        _ => true,
    }
}

/// `shutil.which`: the first executable regular file named `name` on `PATH`.
pub fn which(env: &Environment, name: &str) -> Option<PathBuf> {
    let path = env.get("PATH")?.clone();
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: c_path is a valid NUL-terminated string for the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn session_error(message: String) -> Error {
    Error::new(ErrorKind::SessionError, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn verdict(report: Value, email: &str, org: &str) -> Verdict {
        judge_auth_status(report.as_object().unwrap(), email, org)
    }

    #[test]
    fn auth_status_needs_claude_ai_login_as_the_slot() {
        let ok =
            json!({"loggedIn": true, "authMethod": "claude.ai", "email": "a@x.com", "orgId": "o"});
        assert_eq!(verdict(ok.clone(), "a@x.com", "o"), Verdict::Valid);
        assert_eq!(verdict(ok.clone(), "b@x.com", "o"), Verdict::Invalid);
        assert_eq!(verdict(ok.clone(), "a@x.com", "other"), Verdict::Invalid);
        assert_eq!(verdict(ok, "a@x.com", ""), Verdict::Valid);
        let api = json!({"loggedIn": true, "authMethod": "api_key", "email": "a@x.com"});
        assert_eq!(verdict(api, "a@x.com", "o"), Verdict::Invalid);
        let no_org = json!({"loggedIn": true, "authMethod": "claude.ai", "email": "a@x.com"});
        assert_eq!(verdict(no_org, "a@x.com", "o"), Verdict::Valid);
        let logged_out = json!({"loggedIn": false, "authMethod": "claude.ai", "email": "a@x.com"});
        assert_eq!(verdict(logged_out, "a@x.com", "o"), Verdict::Invalid);
    }
}
