//! Black-box harness for the `ccswap` binary (seam 1 of #1).
//!
//! Every test gets a [`TestEnv`]: a throwaway `HOME`, a fake `security`, a stub
//! HTTP server and a fake `claude` first on `PATH`. The binary is launched with
//! a cleared environment that points every seam at those doubles, so nothing
//! can reach the real `~/.claude-swap-backup`, `~/.claude*` or Keychain.
//!
//! ```ignore
//! mod support;
//! use support::TestEnv;
//!
//! let env = TestEnv::new();
//! env.seed_login("a@example.com", "org-1", "Acme");
//! env.seed_sequence(serde_json::json!({ "accounts": { /* ... */ } }));
//! env.security().set_item("Claude Code-credentials", support::KEYCHAIN_ACCOUNT, "...");
//! env.http().respond("GET", "/api/oauth/usage", StubResponse::json(200, "{}"));
//!
//! let out = env.run(&["status", "--json"]);
//! assert_eq!(out.code, 0);
//! let calls = env.security().calls();        // argv + stdin of each `security` call
//! let reqs = env.http().requests();           // method, path, headers, body
//! ```
//!
//! - Seed files with [`TestEnv::write_json`] / [`TestEnv::seed_sequence`] /
//!   [`TestEnv::seed_login`]. Use timestamps relative to now (upstream `_iso_in`);
//!   the clock is never faked.
//! - [`TestEnv::cmd`] returns a builder for extra env vars, stdin, or running
//!   as root (`.sudo()`, see [`sudo_available`]).
//! - `CLAUDE_CONFIG_DIR` is unset by default (the default profile). Call
//!   [`TestEnv::use_custom_config_dir`] to point it into the temp dir.
//! - Differential runs: set `CCSWAP_TEST_UPSTREAM=<path to the upstream
//!   checkout>` and the same tests run against Python `cswap` through
//!   `upstream_shim.py` (needs `uv`). Branch on [`is_upstream`] / [`prog`] only
//!   for intended differences such as the binary name.
//! - `env.http().respond_with(method, path, |req| ...)` runs code when the
//!   request arrives (e.g. a `/login` landing mid-request);
//!   `env.security().fail_command("add-generic-password", 51)` fails one
//!   `security` command only; `fail_command_for_service` /
//!   `fail_command_for_item` narrow it to one service / item.
//! - `env.cmd(..).using(Impl::Ccswap | Impl::Upstream)` pins one
//!   implementation for cswap/ccswap round trips (Upstream needs
//!   `CCSWAP_TEST_UPSTREAM`).
//! - The isolation guard ([`check_isolation`]) runs before every launch and
//!   refuses to start the binary unless `HOME`, the `security` path and every
//!   HTTP endpoint point at this env's doubles.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use serde_json::Value;

pub const KEYCHAIN_ACCOUNT: &str = "ccswap-test";
pub const ACTIVE_OAUTH_SERVICE: &str = "Claude Code-credentials";
pub const MANAGED_KEY_SERVICE: &str = "Claude Code";

pub const SECURITY_ENV: &str = "CCSWAP_SECURITY_PATH";
pub const HTTP_ENVS: [(&str, &str); 3] = [
    ("CCSWAP_API_BASE_URL", ""),
    ("CCSWAP_OAUTH_TOKEN_URL", "/v1/oauth/token"),
    ("CCSWAP_UPDATE_CHECK_URL", "/update-check"),
];
const UPSTREAM_ENV: &str = "CCSWAP_TEST_UPSTREAM";
const UPSTREAM_SRC_ENV: &str = "CCSWAP_UPSTREAM_SRC";

const FAKE_SECURITY: &str = include_str!("fake_security.py");
const FAKE_CLAUDE: &str = include_str!("fake_claude.py");
const UPSTREAM_SHIM: &str = include_str!("upstream_shim.py");

pub fn is_upstream() -> bool {
    std::env::var_os(UPSTREAM_ENV).is_some_and(|v| !v.is_empty())
}

/// The program name the binary under test reports (`ccswap`, or `cswap` upstream).
pub fn prog() -> &'static str {
    if is_upstream() { "cswap" } else { "ccswap" }
}

pub fn sudo_available() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub struct TestEnv {
    root: tempfile::TempDir,
    home: PathBuf,
    config_dir: Option<PathBuf>,
    security: FakeSecurity,
    claude: FakeClaude,
    http: StubServer,
}

impl Default for TestEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl TestEnv {
    pub fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("ccswap-test-")
            .tempdir()
            .expect("create temp root");
        let base = root.path().canonicalize().expect("canonical temp root");
        let home = base.join("home");
        mkdir_private(&home.join(".claude"));
        let security = FakeSecurity::install(&base.join("fake-security"));
        let claude = FakeClaude::install(&base.join("fake-claude"));
        let http = StubServer::start();
        TestEnv {
            root,
            home,
            config_dir: None,
            security,
            claude,
            http,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.home.parent().unwrap().to_path_buf()
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `~/.claude-swap-backup` inside the temp `HOME`.
    pub fn store_dir(&self) -> PathBuf {
        self.home.join(".claude-swap-backup")
    }

    /// Claude Code's config home: `CLAUDE_CONFIG_DIR` when set, else `~/.claude`.
    pub fn claude_config_home(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".claude"))
    }

    /// Claude Code's global config (`.claude.json`), following the same rule.
    pub fn global_config_path(&self) -> PathBuf {
        match &self.config_dir {
            Some(dir) => dir.join(".claude.json"),
            None => self.home.join(".claude.json"),
        }
    }

    pub fn use_custom_config_dir(&mut self) -> PathBuf {
        let dir = self.root().join("claude-config");
        mkdir_private(&dir);
        self.config_dir = Some(dir.clone());
        dir
    }

    pub fn security(&self) -> &FakeSecurity {
        &self.security
    }

    pub fn claude(&self) -> &FakeClaude {
        &self.claude
    }

    pub fn http(&self) -> &StubServer {
        &self.http
    }

    /// The directory prepended to the binary's `PATH` (holds the fake `claude`).
    pub fn claude_bin_dir(&self) -> &Path {
        &self.claude.bin_dir
    }

    pub fn write_file(&self, path: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.resolve(path.as_ref());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, contents).expect("write seed file");
        path
    }

    pub fn write_json(&self, path: impl AsRef<Path>, value: &Value) -> PathBuf {
        self.write_file(path, &serde_json::to_string_pretty(value).unwrap())
    }

    pub fn read_json(&self, path: impl AsRef<Path>) -> Value {
        let path = self.resolve(path.as_ref());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    }

    /// Write `.claude.json` with an `oauthAccount`, as Claude Code does after `/login`.
    pub fn seed_login(&self, email: &str, org_uuid: &str, org_name: &str) -> PathBuf {
        let mut account = serde_json::json!({
            "emailAddress": email,
            "accountUuid": format!("uuid-{email}"),
        });
        if !org_uuid.is_empty() {
            account["organizationUuid"] = org_uuid.into();
            account["organizationName"] = org_name.into();
        }
        self.write_json(
            self.global_config_path(),
            &serde_json::json!({ "oauthAccount": account }),
        )
    }

    /// Write `sequence.json` into the store with upstream's permissions.
    pub fn seed_sequence(&self, data: Value) -> PathBuf {
        let store = self.store_dir();
        for dir in [
            store.clone(),
            store.join("configs"),
            store.join("credentials"),
        ] {
            mkdir_private(&dir);
        }
        let path = self.write_json(store.join("sequence.json"), &data);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    pub fn cmd<I, S>(&self, args: I) -> Cmd<'_>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Cmd {
            env: self,
            args: args.into_iter().map(|a| a.as_ref().to_string()).collect(),
            vars: self.base_env(),
            stdin: String::new(),
            as_root: false,
            which: Impl::UnderTest,
        }
    }

    pub fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.cmd(args).run()
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.home.join(path)
        }
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        vars.insert(
            "PATH".into(),
            format!("{}:{inherited_path}", self.claude.bin_dir.display()),
        );
        vars.insert("HOME".into(), self.home.display().to_string());
        vars.insert("USER".into(), KEYCHAIN_ACCOUNT.into());
        vars.insert("TMPDIR".into(), self.root().display().to_string());
        vars.insert(
            SECURITY_ENV.into(),
            self.security.path.display().to_string(),
        );
        for (name, path) in HTTP_ENVS {
            vars.insert(name.into(), format!("{}{path}", self.http.base_url()));
        }
        if let Some(dir) = &self.config_dir {
            vars.insert("CLAUDE_CONFIG_DIR".into(), dir.display().to_string());
        }
        vars
    }
}

/// Refuse to launch unless every seam in `vars` points inside `root` or at
/// the local stub server.
pub fn check_isolation(vars: &BTreeMap<String, String>, root: &Path) -> Result<(), String> {
    let inside = |name: &str| -> Result<(), String> {
        let value = vars
            .get(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{name} is not set"))?;
        if Path::new(value).starts_with(root) {
            Ok(())
        } else {
            Err(format!("{name}={value} is outside the test root"))
        }
    };
    inside("HOME")?;
    inside(SECURITY_ENV)?;
    for name in [
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "XDG_DATA_HOME",
    ] {
        if vars.contains_key(name) {
            inside(name)?;
        }
    }
    for (name, _) in HTTP_ENVS {
        match vars.get(name) {
            Some(url) if url.starts_with("http://127.0.0.1:") => {}
            Some(url) => return Err(format!("{name}={url} is not the local stub server")),
            None => return Err(format!("{name} is not set")),
        }
    }
    Ok(())
}

pub struct Cmd<'a> {
    env: &'a TestEnv,
    args: Vec<String>,
    vars: BTreeMap<String, String>,
    stdin: String,
    as_root: bool,
    which: Impl,
}

impl Cmd<'_> {
    /// Launch a specific implementation (see [`Impl`]).
    pub fn using(mut self, which: Impl) -> Self {
        self.which = which;
        self
    }

    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    pub fn env_remove(mut self, name: &str) -> Self {
        self.vars.remove(name);
        self
    }

    pub fn stdin(mut self, input: &str) -> Self {
        self.stdin = input.into();
        self
    }

    /// Run through `sudo -n`; callers must check [`sudo_available`] first.
    pub fn sudo(mut self) -> Self {
        self.as_root = true;
        self
    }

    pub fn run(self) -> Output {
        self.try_run().unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_run(self) -> Result<Output, String> {
        check_isolation(&self.vars, &self.env.root())
            .map_err(|e| format!("harness refused to launch the binary: {e}"))?;
        let (program, mut args, mut vars) = target(&self.env.root(), self.which);
        args.extend(self.args);
        vars.extend(self.vars);
        let mut command = if self.as_root {
            let mut c = Command::new("sudo");
            c.args(["-n", "/usr/bin/env", "-i"]);
            c.args(vars.iter().map(|(k, v)| format!("{k}={v}")));
            c.arg(&program);
            c
        } else {
            let mut c = Command::new(&program);
            c.env_clear().envs(&vars);
            c
        };
        command
            .args(&args)
            .current_dir(&self.env.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", program.display()))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(self.stdin.as_bytes())
            .ok();
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Which implementation a [`Cmd`] launches. [`Impl::UnderTest`] follows
/// `CCSWAP_TEST_UPSTREAM`; the other two pin one side for compatibility tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impl {
    UnderTest,
    Ccswap,
    Upstream,
}

fn upstream_root() -> Option<PathBuf> {
    std::env::var_os(UPSTREAM_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn target(root: &Path, which: Impl) -> (PathBuf, Vec<String>, BTreeMap<String, String>) {
    let upstream = match which {
        Impl::Ccswap => None,
        Impl::UnderTest => upstream_root(),
        Impl::Upstream => Some(upstream_root().expect("CCSWAP_TEST_UPSTREAM is not set")),
    };
    let Some(upstream) = upstream else {
        return (
            PathBuf::from(env!("CARGO_BIN_EXE_ccswap")),
            Vec::new(),
            BTreeMap::new(),
        );
    };
    let python = upstream_python(&upstream);
    let shim = root.join("upstream_shim.py");
    if !shim.exists() {
        std::fs::write(&shim, UPSTREAM_SHIM).expect("write upstream shim");
    }
    let mut vars = BTreeMap::new();
    vars.insert(
        UPSTREAM_SRC_ENV.into(),
        upstream.join("src").display().to_string(),
    );
    let args = vec!["-I".into(), "-B".into(), shim.display().to_string()];
    (python, args, vars)
}

fn upstream_python(upstream: &Path) -> PathBuf {
    static PYTHON: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    PYTHON
        .get_or_init(|| {
            let venv = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("upstream-venv");
            let status = Command::new("uv")
                .args(["sync", "--quiet", "--frozen", "--no-dev", "--no-editable"])
                .arg("--project")
                .arg(upstream)
                .env("UV_PROJECT_ENVIRONMENT", &venv)
                .status()
                .map_err(|e| format!("run uv: {e}"))?;
            if !status.success() {
                return Err(format!("uv sync failed for {}", upstream.display()));
            }
            Ok(venv.join("bin/python"))
        })
        .clone()
        .unwrap_or_else(|e| panic!("{e}"))
}

#[derive(Debug, Clone)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    #[track_caller]
    pub fn assert_code(&self, code: i32) -> &Self {
        assert_eq!(
            self.code, code,
            "exit code\nstdout:\n{}\nstderr:\n{}",
            self.stdout, self.stderr
        );
        self
    }
}

fn mkdir_private(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create dir");
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn install_script(dir: &Path, name: &str, template: &str) -> PathBuf {
    let state = dir.join("state");
    std::fs::create_dir_all(&state).expect("create fake state dir");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin dir");
    let path = bin.join(name);
    let shared = shared_script(name, template);
    if std::fs::hard_link(&shared, &path).is_err() {
        std::fs::copy(&shared, &path).expect("copy fake");
    }
    path
}

/// One copy of each fake per test run, hard-linked into every env: macOS
/// scans each new executable on first exec (~0.3 s, serialized), which under
/// parallel tests pushed `security` calls past their 5 s timeout. The fakes
/// find their state directory from their own (linked) path.
fn shared_script(name: &str, template: &str) -> PathBuf {
    let digest = template.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fakes");
    let path = dir.join(format!("{name}-{digest:016x}"));
    if !path.exists() {
        std::fs::create_dir_all(&dir).expect("create shared fakes dir");
        // Parallel test threads share the pid, so each call needs its own temp name.
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = dir.join(format!("{name}.{}.{seq}.tmp", std::process::id()));
        std::fs::write(&temp, template).expect("write fake");
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&temp, &path).expect("publish fake");
    }
    path
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("jsonl line"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// `security` double: items keyed by `(service, account)`, exit 44 for a
/// missing item, the secret printed only with `-w`, and `-i` commands read
/// from stdin.
#[derive(Clone)]
pub struct FakeSecurity {
    path: PathBuf,
    state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SecurityCall {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    /// Whether each path given to [`FakeSecurity::observe`] existed when the call ran.
    pub observed: BTreeMap<String, bool>,
}

impl FakeSecurity {
    fn install(dir: &Path) -> Self {
        let path = install_script(dir, "security", FAKE_SECURITY);
        FakeSecurity {
            path,
            state: dir.join("state"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_item(&self, service: &str, account: &str, password: &str) {
        let mut items = self.raw_items();
        items.retain(|i| !(i["service"] == service && i["account"] == account));
        items.push(serde_json::json!({
            "service": service, "account": account, "password": password,
        }));
        self.write_items(&items);
    }

    pub fn item(&self, service: &str, account: &str) -> Option<String> {
        self.raw_items()
            .into_iter()
            .find(|i| i["service"] == service && i["account"] == account)
            .map(|i| i["password"].as_str().unwrap().to_string())
    }

    /// `(service, account, password)` for every stored item.
    pub fn items(&self) -> Vec<(String, String, String)> {
        self.raw_items()
            .into_iter()
            .map(|i| {
                let s = |k: &str| i[k].as_str().unwrap().to_string();
                (s("service"), s("account"), s("password"))
            })
            .collect()
    }

    /// Make every later call fail with `rc` (e.g. 51: locked / interaction not allowed).
    pub fn fail_with(&self, rc: i32) {
        std::fs::write(self.state.join("fail_rc"), rc.to_string()).unwrap();
    }

    /// Make only `command` (e.g. `add-generic-password`) fail with `rc`,
    /// including when it arrives through `security -i`.
    pub fn fail_command(&self, command: &str, rc: i32) {
        std::fs::write(self.state.join(format!("fail_{command}")), rc.to_string()).unwrap();
    }

    /// Like [`Self::fail_command`], only for items of `service`.
    pub fn fail_command_for_service(&self, command: &str, service: &str, rc: i32) {
        std::fs::write(
            self.state.join(format!("fail_{command}@{service}")),
            rc.to_string(),
        )
        .unwrap();
    }

    /// Like [`Self::fail_command`], only for one `(service, account)` item.
    pub fn fail_command_for_item(&self, command: &str, service: &str, account: &str, rc: i32) {
        std::fs::write(
            self.state
                .join(format!("fail_{command}@{service}@{account}")),
            rc.to_string(),
        )
        .unwrap();
    }

    /// Record, on every later call, whether each of `paths` exists (e.g. a lock).
    pub fn observe(&self, paths: &[&Path]) {
        let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        std::fs::write(
            self.state.join("observe.json"),
            serde_json::to_string(&list).unwrap(),
        )
        .unwrap();
    }

    /// Hold the next `command` call (also via `security -i`) until [`Self::release`],
    /// for at most 4.5 s since callers time out at 5 s.
    pub fn pause_next(&self, command: &str) {
        let _ = std::fs::remove_file(self.state.join("release"));
        std::fs::write(self.state.join(format!("pause_{command}")), "").unwrap();
    }

    /// Wait until a call is held by [`Self::pause_next`]; false on timeout.
    pub fn wait_until_paused(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !self.state.join("paused").exists() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        true
    }

    pub fn release(&self) {
        std::fs::write(self.state.join("release"), "").unwrap();
    }

    pub fn calls(&self) -> Vec<SecurityCall> {
        read_jsonl(&self.state.join("calls.jsonl"))
            .into_iter()
            .map(|c| SecurityCall {
                argv: serde_json::from_value(c["argv"].clone()).unwrap(),
                stdin: c["stdin"].as_str().map(str::to_string),
                observed: serde_json::from_value(c["observed"].clone()).unwrap_or_default(),
            })
            .collect()
    }

    fn raw_items(&self) -> Vec<Value> {
        match std::fs::read_to_string(self.state.join("items.json")) {
            Ok(text) => serde_json::from_str(&text).unwrap(),
            Err(_) => Vec::new(),
        }
    }

    fn write_items(&self, items: &[Value]) {
        std::fs::write(
            self.state.join("items.json"),
            serde_json::to_string_pretty(items).unwrap(),
        )
        .unwrap();
    }
}

/// `claude` double placed first on `PATH`. Records argv, cwd and the full
/// environment of each call; replies are configured per joined argv.
pub struct FakeClaude {
    bin_dir: PathBuf,
    state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClaudeCall {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
}

impl FakeClaude {
    fn install(dir: &Path) -> Self {
        install_script(dir, "claude", FAKE_CLAUDE);
        FakeClaude {
            bin_dir: dir.join("bin"),
            state: dir.join("state"),
        }
    }

    /// Reply to `claude <args>` (e.g. `"auth status --json"`) with this output.
    pub fn respond(&self, args: &str, stdout: &str, exit: i32) {
        let path = self.state.join("responses.json");
        let mut responses: serde_json::Map<String, Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        responses.insert(
            args.into(),
            serde_json::json!({ "stdout": stdout, "exit": exit }),
        );
        std::fs::write(&path, Value::Object(responses).to_string()).unwrap();
    }

    pub fn calls(&self) -> Vec<ClaudeCall> {
        read_jsonl(&self.state.join("calls.jsonl"))
            .into_iter()
            .map(|c| ClaudeCall {
                argv: serde_json::from_value(c["argv"].clone()).unwrap(),
                cwd: c["cwd"].as_str().unwrap().to_string(),
                env: serde_json::from_value(c["env"].clone()).unwrap(),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct StubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl StubResponse {
    pub fn json(status: u16, body: &str) -> Self {
        StubResponse {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone)]
pub struct StubRequest {
    pub method: String,
    /// Path including any query string.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl StubRequest {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

type Handler = Arc<dyn Fn(&StubRequest) -> StubResponse + Send + Sync>;
type Routes = Arc<Mutex<BTreeMap<(String, String), Handler>>>;

/// Local HTTP/1.1 server. Unrouted requests get 404 and are still recorded.
pub struct StubServer {
    addr: std::net::SocketAddr,
    routes: Routes,
    requests: Arc<Mutex<Vec<StubRequest>>>,
    stop: Arc<AtomicBool>,
}

impl StubServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let addr = listener.local_addr().unwrap();
        let routes: Routes = Arc::default();
        let requests: Arc<Mutex<Vec<StubRequest>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (r, q, s) = (routes.clone(), requests.clone(), stop.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    let (r, q) = (r.clone(), q.clone());
                    thread::spawn(move || serve(stream, &r, &q));
                }
            }
        });
        StubServer {
            addr,
            routes,
            requests,
            stop,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Answer `method path` (path without query) with `response` from now on.
    pub fn respond(&self, method: &str, path: &str, response: StubResponse) {
        self.respond_with(method, path, move |_| response.clone());
    }

    /// Answer `method path` by running `handler` when the request arrives,
    /// e.g. to change files while the binary waits on the network.
    pub fn respond_with<F>(&self, method: &str, path: &str, handler: F)
    where
        F: Fn(&StubRequest) -> StubResponse + Send + Sync + 'static,
    {
        self.routes
            .lock()
            .unwrap()
            .insert((method.to_uppercase(), path.into()), Arc::new(handler));
    }

    pub fn requests(&self) -> Vec<StubRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(stream: TcpStream, routes: &Routes, requests: &Mutex<Vec<StubRequest>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    let _ = reader.read_exact(&mut body);
    let request = StubRequest {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    let handler = routes
        .lock()
        .unwrap()
        .get(&(request.method.clone(), request.path().to_string()))
        .cloned();
    let response = match handler {
        Some(handler) => handler(&request),
        None => StubResponse::json(404, r#"{"error":"not stubbed"}"#),
    };
    requests.lock().unwrap().push(request);
    let mut stream = reader.into_inner();
    let mut head = format!(
        "HTTP/1.1 {} Stub\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (k, v) in &response.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(response.body.as_bytes());
}
