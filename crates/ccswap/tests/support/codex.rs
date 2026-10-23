//! Codex side of the harness: the live `auth.json`, the Codex store and the fake `codex`.
//!
//! ```ignore
//! let env = TestEnv::new();
//! let auth = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
//! env.codex().script_login(Some(&CodexAuth::new("b@example.com", "acct-b").render()), 0);
//! env.codex().app_server(json!({ "results": { "account/rateLimits/read": { /* ... */ } } }));
//! env.http().respond("GET", "/backend-api/wham/usage", StubResponse::json(200, "{}"));
//! let out = env.run(["codex", "add"]);
//! assert_eq!(env.codex_managed_auth(1).as_deref(), Some(auth.as_str()));
//! ```

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{TestEnv, install_fake, mkdir_private, read_jsonl};

impl TestEnv {
    /// `CODEX_HOME` when [`use_custom_codex_home`](Self::use_custom_codex_home) set it, else `~/.codex`.
    pub fn codex_home(&self) -> PathBuf {
        self.codex_home
            .clone()
            .unwrap_or_else(|| self.home.join(".codex"))
    }

    pub fn use_custom_codex_home(&mut self) -> PathBuf {
        let dir = self.root().join("codex-home");
        mkdir_private(&dir);
        self.codex_home = Some(dir.clone());
        dir
    }

    pub fn live_codex_auth_path(&self) -> PathBuf {
        self.codex_home().join("auth.json")
    }

    /// Writes the live `auth.json` and returns its exact bytes.
    pub fn seed_codex_login(&self, auth: &CodexAuth) -> String {
        let text = auth.render();
        self.seed_live_codex_auth(&text);
        text
    }

    pub fn seed_live_codex_auth(&self, text: &str) {
        let path = self.live_codex_auth_path();
        mkdir_private(path.parent().unwrap());
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    pub fn codex_store_dir(&self) -> PathBuf {
        self.store_dir().join("codex")
    }

    pub fn codex_ledger_path(&self) -> PathBuf {
        self.codex_store_dir().join("accounts.json")
    }

    pub fn codex_ledger(&self) -> Value {
        self.read_json(self.codex_ledger_path())
    }

    pub fn seed_codex_ledger(&self, ledger: Value) {
        mkdir_private(&self.codex_store_dir());
        let path = self.write_json(self.codex_ledger_path(), &ledger);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    pub fn codex_managed_home(&self, number: u32) -> PathBuf {
        self.codex_store_dir()
            .join("homes")
            .join(number.to_string())
    }

    pub fn codex_managed_auth(&self, number: u32) -> Option<String> {
        std::fs::read_to_string(self.codex_managed_home(number).join("auth.json")).ok()
    }

    /// Seeds a ledger row and its managed home in one go, for tests that start from a registered account.
    pub fn seed_codex_account(&self, number: u32, auth: &CodexAuth) -> String {
        let text = auth.render();
        let home = self.codex_managed_home(number);
        mkdir_private(&home);
        std::fs::write(home.join("auth.json"), &text).unwrap();
        std::fs::set_permissions(
            home.join("auth.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let path = self.codex_ledger_path();
        let mut ledger = if path.exists() {
            self.codex_ledger()
        } else {
            json!({ "active": null, "sequence": [], "accounts": {} })
        };
        let sequence = ledger["sequence"].as_array_mut().unwrap();
        if !sequence.contains(&number.into()) {
            sequence.push(number.into());
        }
        ledger["accounts"][number.to_string()] = json!({
            "email": auth.email,
            "account_id": auth.account_id.to_lowercase(),
            "plan": auth.plan,
            "alias": null,
            "added": "2026-01-01T00:00:00Z",
        });
        self.seed_codex_ledger(ledger);
        text
    }
}

/// An `auth.json` as `codex login` writes it, with an unsigned id_token carrying the identity claims.
#[derive(Debug, Clone)]
pub struct CodexAuth {
    pub email: String,
    pub account_id: String,
    pub plan: String,
    pub tag: String,
    pub token_account_id: bool,
    pub tokens: bool,
    pub access_expires_at: i64,
}

impl CodexAuth {
    pub fn new(email: &str, account_id: &str) -> Self {
        CodexAuth {
            email: email.into(),
            account_id: account_id.into(),
            plan: "plus".into(),
            tag: "v1".into(),
            token_account_id: true,
            tokens: true,
            access_expires_at: 4_102_444_800,
        }
    }

    pub fn plan(mut self, plan: &str) -> Self {
        self.plan = plan.into();
        self
    }

    /// Distinguishes generations of the same account's tokens.
    pub fn tag(mut self, tag: &str) -> Self {
        self.tag = tag.into();
        self
    }

    /// Leaves `tokens.account_id` out so the id_token's `chatgpt_account_id` is the only source.
    pub fn without_token_account_id(mut self) -> Self {
        self.token_account_id = false;
        self
    }

    /// An API-key login: `OPENAI_API_KEY` only, no `tokens`.
    pub fn api_key_only(mut self) -> Self {
        self.tokens = false;
        self
    }

    pub fn access_expires_at(mut self, unix: i64) -> Self {
        self.access_expires_at = unix;
        self
    }

    pub fn access_token(&self) -> String {
        jwt(&json!({ "exp": self.access_expires_at, "tag": self.tag }))
    }

    pub fn refresh_token(&self) -> String {
        format!("rt-{}-{}", self.account_id, self.tag)
    }

    pub fn render(&self) -> String {
        if !self.tokens {
            return serde_json::to_string_pretty(&json!({
                "OPENAI_API_KEY": "sk-proj-test",
                "tokens": null,
                "last_refresh": null,
            }))
            .unwrap();
        }
        let id_token = jwt(&json!({
            "email": self.email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": self.account_id,
                "chatgpt_plan_type": self.plan,
            },
        }));
        let mut tokens = json!({
            "id_token": id_token,
            "access_token": self.access_token(),
            "refresh_token": self.refresh_token(),
        });
        if self.token_account_id {
            tokens["account_id"] = self.account_id.clone().into();
        }
        serde_json::to_string_pretty(&json!({
            "OPENAI_API_KEY": null,
            "tokens": tokens,
            "last_refresh": "2026-01-01T00:00:00Z",
        }))
        .unwrap()
    }
}

fn jwt(claims: &Value) -> String {
    format!(
        "{}.{}.sig",
        base64url(br#"{"alg":"none","typ":"JWT"}"#),
        base64url(claims.to_string().as_bytes())
    )
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// See `crates/ccswap-test-fakes/src/codex.rs` for the state files it reads.
pub struct FakeCodex {
    pub(super) bin_dir: PathBuf,
    state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CodexCall {
    pub argv: Vec<String>,
    pub codex_home: Option<String>,
    /// `CODEX_HOME` was a directory when codex started.
    pub codex_home_existed: bool,
    pub cwd: String,
}

impl FakeCodex {
    pub(super) fn install(dir: &Path) -> Self {
        install_fake(dir, "codex");
        FakeCodex {
            bin_dir: dir.join("bin"),
            state: dir.join("state"),
        }
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    /// Answers exactly this argv (joined by spaces) before any modelled behaviour.
    pub fn respond(&self, args: &str, stdout: &str, exit: i32) {
        self.update_json("responses.json", json!({}), |responses| {
            responses[args] = json!({ "stdout": stdout, "exit": exit });
        });
    }

    /// Queues one `codex login`: writes `auth` to `$CODEX_HOME/auth.json` (when given) and exits with `exit`.
    pub fn script_login(&self, auth: Option<&str>, exit: i32) {
        self.update_json("login.json", json!([]), |queue| {
            queue
                .as_array_mut()
                .unwrap()
                .push(json!({ "auth": auth, "exit": exit }));
        });
    }

    /// `{auth?, results: {method: result}, errors: {method: {code, message}}, hang?}` for
    /// `codex app-server`; `hang` spawns a child, then answers nothing and ignores stdin.
    pub fn app_server(&self, script: Value) {
        std::fs::write(self.state.join("app_server.json"), script.to_string()).unwrap();
    }

    /// Queues live logins the fake writes when `switch` runs its mid-switch hook; pass
    /// [`switch_hook_env`](Self::switch_hook_env) to that run.
    pub fn rewrite_live_during_switch(&self, auth: &str) {
        self.update_json("switch_hook.json", json!([]), |queue| {
            queue.as_array_mut().unwrap().push(auth.into());
        });
    }

    pub fn switch_hook_env(&self) -> (&'static str, String) {
        (
            "CCSWAP_TEST_CODEX_SWITCH_HOOK",
            self.bin_dir.join("codex").display().to_string(),
        )
    }

    pub fn calls(&self) -> Vec<CodexCall> {
        read_jsonl(&self.state.join("calls.jsonl"))
            .into_iter()
            .map(|c| CodexCall {
                argv: serde_json::from_value(c["argv"].clone()).unwrap(),
                codex_home: c["codexHome"].as_str().map(str::to_string),
                codex_home_existed: c["codexHomeExisted"].as_bool().unwrap_or(false),
                cwd: c["cwd"].as_str().unwrap().to_string(),
            })
            .collect()
    }

    pub fn app_server_requests(&self) -> Vec<Value> {
        read_jsonl(&self.state.join("app_server_requests.jsonl"))
    }

    /// Pids a `hang` app-server recorded: its own, then the child it spawned.
    pub fn app_server_pids(&self) -> Vec<i64> {
        read_jsonl(&self.state.join("app_server_pids.jsonl"))
            .iter()
            .filter_map(Value::as_i64)
            .collect()
    }

    fn update_json(&self, name: &str, empty: Value, edit: impl FnOnce(&mut Value)) {
        let path = self.state.join(name);
        let mut value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(empty);
        edit(&mut value);
        std::fs::write(&path, value.to_string()).unwrap();
    }
}
