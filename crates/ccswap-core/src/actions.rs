//! What interactive surfaces may do: the snapshot read plus
//! the account operations, behind one trait so a surface can be driven by a
//! fake (the range of upstream's test `FakeSwitcher`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use crate::accounts::{Accounts, AddOptions, AddTokenOptions, Console, TokenInput};
use crate::anthropic_client::AnthropicClient;
use crate::endpoints::Endpoints;
use crate::env::Environment;
use crate::error::Result;
use crate::paths::Paths;
use crate::presentation::{Style, Theme};
use crate::secret_store::{Keychain, SecretStore};
use crate::snapshot::{self, AccountsSnapshot};
use crate::store;
use crate::switcher::{SwitchStrategy, Switcher};

/// Blocking calls (file locks, Keychain subprocesses, network): run them
/// off the UI thread. Human-readable output goes to `console`; switches
/// return their `--json` payload.
pub trait Actions: Send + Sync {
    fn backup_dir(&self) -> PathBuf;
    fn accounts_snapshot(&self, fetch: Option<&HashSet<String>>) -> Result<AccountsSnapshot>;
    /// The live login's slot; `None` when there is none or it is unmanaged.
    fn current_account_number(&self) -> Option<String>;
    fn switch_to(
        &self,
        identifier: &str,
        force: bool,
        console: &mut dyn Console,
    ) -> Result<Option<Value>>;
    fn switch(
        &self,
        strategy: Option<&SwitchStrategy>,
        console: &mut dyn Console,
    ) -> Result<Option<Value>>;
    fn remove_account(
        &self,
        identifier: &str,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()>;
    fn set_account_disabled(
        &self,
        identifier: &str,
        disabled: bool,
        console: &mut dyn Console,
    ) -> Result<()>;
    fn add_account(
        &self,
        slot: Option<i64>,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()>;
    fn add_account_from_token(
        &self,
        token: &str,
        email: Option<&str>,
        slot: Option<i64>,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()>;
    /// Pins the poll-planning threshold and models (a hosted auto engine's
    /// effective settings) instead of reading settings.json.
    fn set_poll_policy_inputs(&self, threshold: f64, models: Vec<String>);
    fn clear_poll_policy_inputs(&self);
    /// The live login's email from the Claude Code config alone: no
    /// Keychain, cheap enough to poll.
    fn live_email(&self) -> Option<String> {
        None
    }
    /// The Claude Code config a login rewrites; its mtime signals a switch
    /// made elsewhere.
    fn claude_config_path(&self) -> Option<PathBuf> {
        None
    }
}

/// The outcome of one captured action.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionResult {
    pub ok: bool,
    pub output: Vec<String>,
    pub payload: Option<Value>,
}

impl ActionResult {
    /// The first non-empty output line without ANSI styling.
    pub fn first_line(&self) -> String {
        self.output
            .iter()
            .flat_map(|l| l.split('\n'))
            .map(|l| strip_ansi(l).trim().to_string())
            .find(|l| !l.is_empty())
            .unwrap_or_default()
    }

    pub fn output_text(&self) -> String {
        self.output.join("\n")
    }
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Collects output; any prompt is answered with end of input.
#[derive(Default)]
struct CaptureConsole {
    lines: Vec<String>,
    prompted: bool,
}

impl Console for CaptureConsole {
    fn line(&mut self, text: &str) {
        self.lines.push(text.to_string());
    }

    fn prompt(&mut self, question: &str) -> Option<String> {
        if !question.is_empty() {
            self.lines.push(question.to_string());
        }
        self.prompted = true;
        None
    }
}

/// Runs an action with its output captured. Nothing may prompt: input is
/// not available inside a surface, so a prompt fails the action.
pub fn run_action(f: impl FnOnce(&mut dyn Console) -> Result<Option<Value>>) -> ActionResult {
    let mut console = CaptureConsole::default();
    let result = f(&mut console);
    let mut output = console.lines;
    if console.prompted {
        output.push("Error: interactive input is not available here.".into());
        return ActionResult {
            ok: false,
            output,
            payload: None,
        };
    }
    match result {
        Ok(payload) => ActionResult {
            ok: true,
            output,
            payload: payload.filter(Value::is_object),
        },
        Err(error) => {
            output.push(format!("Error: {error}"));
            ActionResult {
                ok: false,
                output,
                payload: None,
            }
        }
    }
}

/// The real store, Keychain and network behind [`Actions`].
pub struct LiveActions {
    paths: Paths,
    env: Environment,
    endpoints: Endpoints,
    style: Style,
    poll_inputs: Mutex<Option<(f64, Vec<String>)>>,
}

impl LiveActions {
    pub fn new(env: Environment) -> Self {
        LiveActions {
            paths: Paths::new(&env),
            endpoints: Endpoints::new(&env),
            style: Style::new(false, Theme::Dark),
            env,
            poll_inputs: Mutex::new(None),
        }
    }

    fn secrets(&self) -> SecretStore {
        let keychain = Keychain::new(self.endpoints.security.clone(), &self.env);
        SecretStore::new(keychain, self.paths.clone(), &self.env)
    }

    fn client(&self) -> AnthropicClient {
        AnthropicClient::new(self.endpoints.clone())
    }

    fn with_accounts<T>(&self, f: impl FnOnce(&Accounts) -> Result<T>) -> Result<T> {
        let (secrets, client) = (self.secrets(), self.client());
        f(&Accounts {
            paths: &self.paths,
            env: &self.env,
            secrets: &secrets,
            client: &client,
            style: &self.style,
        })
    }

    fn with_switcher<T>(&self, f: impl FnOnce(&Switcher) -> Result<T>) -> Result<T> {
        let (secrets, client) = (self.secrets(), self.client());
        f(&Switcher {
            paths: &self.paths,
            env: &self.env,
            secrets: &secrets,
            client: &client,
            style: &self.style,
            listing: None,
        })
    }
}

impl Actions for LiveActions {
    fn backup_dir(&self) -> PathBuf {
        self.paths.backup_root()
    }

    fn accounts_snapshot(&self, fetch: Option<&HashSet<String>>) -> Result<AccountsSnapshot> {
        let pinned = self
            .poll_inputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        snapshot::accounts_snapshot(
            &self.paths,
            &self.env,
            &self.secrets(),
            &self.client(),
            fetch,
            pinned.as_ref(),
        )
    }

    fn current_account_number(&self) -> Option<String> {
        let identity = store::current_identity(&self.paths)?;
        let data = store::read_sequence(&self.paths).ok()??;
        store::find_account_slot(&data, &identity.email, &identity.organization_uuid)
    }

    fn switch_to(
        &self,
        identifier: &str,
        force: bool,
        console: &mut dyn Console,
    ) -> Result<Option<Value>> {
        self.with_switcher(|s| {
            Ok(s.switch_to(identifier, true, force, console)?
                .map(|r| r.to_json()))
        })
    }

    fn switch(
        &self,
        strategy: Option<&SwitchStrategy>,
        console: &mut dyn Console,
    ) -> Result<Option<Value>> {
        self.with_switcher(|s| Ok(s.switch_next(strategy, true, console)?.map(|r| r.to_json())))
    }

    fn remove_account(
        &self,
        identifier: &str,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        self.with_accounts(|a| a.remove_account(identifier, assume_yes, console))
    }

    fn set_account_disabled(
        &self,
        identifier: &str,
        disabled: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        self.with_accounts(|a| a.set_account_disabled(identifier, disabled, console))
    }

    fn add_account(
        &self,
        slot: Option<i64>,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        let options = AddOptions {
            slot,
            alias: None,
            assume_yes,
        };
        self.with_accounts(|a| a.add_account(&options, console))
    }

    fn add_account_from_token(
        &self,
        token: &str,
        email: Option<&str>,
        slot: Option<i64>,
        assume_yes: bool,
        console: &mut dyn Console,
    ) -> Result<()> {
        let options = AddTokenOptions {
            email: email.map(str::to_string),
            slot,
            assume_yes,
        };
        self.with_accounts(|a| a.add_token(TokenInput::Value(token.to_string()), &options, console))
    }

    fn set_poll_policy_inputs(&self, threshold: f64, models: Vec<String>) {
        *self.poll_inputs.lock().unwrap_or_else(|e| e.into_inner()) = Some((threshold, models));
    }

    fn clear_poll_policy_inputs(&self) {
        *self.poll_inputs.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn live_email(&self) -> Option<String> {
        store::current_identity(&self.paths).map(|i| i.email)
    }

    fn claude_config_path(&self) -> Option<PathBuf> {
        Some(self.paths.global_config_path())
    }
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::TestRunAction.
    use super::*;
    use crate::error::{Error, ErrorKind};
    use crate::snapshot::AccountKind;
    use serde_json::json;

    #[test]
    fn captures_output_and_payload() {
        let result = run_action(|c| {
            c.line("hello");
            Ok(Some(json!({"switched": true})))
        });
        assert!(result.ok);
        assert_eq!(result.payload, Some(json!({"switched": true})));
        assert!(result.output_text().contains("hello"));
    }

    #[test]
    fn switch_error_is_captured_not_raised() {
        let result = run_action(|_| Err(Error::new(ErrorKind::ClaudeSwitchError, "boom")));
        assert!(!result.ok);
        assert!(result.output_text().contains("boom"));
    }

    #[test]
    fn unexpected_input_becomes_end_of_input() {
        let result = run_action(|c| {
            c.prompt("should not block");
            Ok(None)
        });
        assert!(!result.ok);
        assert!(result.output_text().contains("interactive input"));
    }

    #[test]
    fn first_line_strips_ansi() {
        let result = run_action(|c| {
            c.line("");
            c.line("\x1b[1mBold headline\x1b[0m");
            Ok(None)
        });
        assert_eq!(result.first_line(), "Bold headline");
    }

    /// upstream TestAccountsSnapshot::test_one_pass_snapshot
    #[test]
    fn live_snapshot_is_one_pass_over_the_store() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::from_pairs([
            ("HOME", home.path().as_os_str().to_owned()),
            ("CCSWAP_SECURITY_PATH", "/usr/bin/false".into()),
            ("CCSWAP_API_BASE_URL", "http://127.0.0.1:9".into()),
        ]);
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"oauthAccount": {"emailAddress": "test@example.com", "accountUuid": "test-uuid-1234"}}"#,
        )
        .unwrap();
        let backup = home.path().join(".claude-swap-backup");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(
            backup.join("sequence.json"),
            r#"{"activeAccountNumber": 1, "sequence": [1, 2], "accounts": {
                "1": {"email": "test@example.com", "uuid": "test-uuid-1234"},
                "2": {"email": "other@example.com", "uuid": "uuid-2", "kind": "api_key"}}}"#,
        )
        .unwrap();
        let live = LiveActions::new(env);

        let snap = live.accounts_snapshot(Some(&HashSet::new())).unwrap();
        assert_eq!(snap.active_number.as_deref(), Some("1"));
        let numbers: Vec<_> = snap.accounts.iter().map(|a| a.number.as_str()).collect();
        assert_eq!(numbers, ["1", "2"]);
        assert!(snap.accounts[0].is_active && snap.accounts[0].email == "test@example.com");
        assert_eq!(
            snap.accounts.iter().map(|a| a.kind).collect::<Vec<_>>(),
            [AccountKind::OAuth, AccountKind::ApiKey]
        );
        assert!(snap.accounts.iter().all(|a| !a.switchable));
        assert!(snap.accounts.iter().all(|a| a.usage.sentinel.is_some()));
        assert!(snap.taken_at > 0.0);
        assert_eq!(live.current_account_number().as_deref(), Some("1"));
        assert_eq!(live.live_email().as_deref(), Some("test@example.com"));
        assert_eq!(
            live.claude_config_path(),
            Some(home.path().join(".claude.json"))
        );
    }

    #[test]
    fn live_snapshot_without_a_store_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::from_pairs([
            ("HOME", home.path().as_os_str().to_owned()),
            ("CCSWAP_SECURITY_PATH", "/usr/bin/false".into()),
        ]);
        let snap = LiveActions::new(env)
            .accounts_snapshot(Some(&HashSet::new()))
            .unwrap();
        assert_eq!(snap.active_number, None);
        assert!(snap.accounts.is_empty());
    }

    #[test]
    fn non_object_payload_is_dropped() {
        assert_eq!(run_action(|_| Ok(Some(json!([1])))).payload, None);
    }
}
