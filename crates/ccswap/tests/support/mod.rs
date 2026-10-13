//! Black-box harness for the `ccswap` binary.
//!
//! Each [`TestEnv`] owns a throwaway `HOME`, a fake `security`, a stub HTTP
//! server and fake `claude` / `launchctl` on `PATH`; [`check_isolation`] refuses
//! to launch unless every seam points at them, so tests never touch the real
//! store, `~/.claude*` or Keychain.
//!
//! ```ignore
//! let env = TestEnv::new();
//! env.seed_login("a@example.com", "org-1", "Acme");
//! env.seed_sequence(serde_json::json!({ "accounts": { /* ... */ } }));
//! env.security().set_item("Claude Code-credentials", support::KEYCHAIN_ACCOUNT, "...");
//! env.http().respond("GET", "/api/oauth/usage", StubResponse::json(200, "{}"));
//!
//! let out = env.run(&["status", "--json"]);
//! let calls = env.security().calls();
//! let reqs = env.http().requests();
//! ```
//!
//! - Use timestamps relative to now; the clock is never faked.
//! - [`TestEnv::cmd`] adds env vars, stdin, `.sudo()` or `.using(Impl::..)`.
//! - `CLAUDE_CONFIG_DIR` is unset unless [`TestEnv::use_custom_config_dir`] is called.
//! - Differential runs: set `CCSWAP_TEST_UPSTREAM=<upstream checkout>` to run the
//!   same tests against Python `cswap` (needs `uv`); branch on [`is_upstream`],
//!   [`prog`] or [`hint`] only for intended differences such as the binary name.

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
const FAKE_LAUNCHCTL: &str = include_str!("fake_launchctl.py");
const UPSTREAM_SHIM: &str = include_str!("upstream_shim.py");

pub fn is_upstream() -> bool {
    std::env::var_os(UPSTREAM_ENV).is_some_and(|v| !v.is_empty())
}

pub fn prog() -> &'static str {
    if is_upstream() { "cswap" } else { "ccswap" }
}

pub fn hint(text: &str) -> String {
    if is_upstream() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("cswap ") {
        let boundary = rest[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || "_.-/".contains(c)));
        out.push_str(&rest[..at]);
        if boundary && !rest[at..].starts_with("cswap run session") {
            out.push('c');
        }
        out.push_str("cswap ");
        rest = &rest[at + "cswap ".len()..];
    }
    out.push_str(rest);
    out
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
    launchctl: FakeLaunchctl,
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
        let launchctl = FakeLaunchctl::install(&base.join("fake-launchctl"));
        let http = StubServer::start();
        TestEnv {
            root,
            home,
            config_dir: None,
            security,
            claude,
            launchctl,
            http,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.home.parent().unwrap().to_path_buf()
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn store_dir(&self) -> PathBuf {
        self.home.join(".claude-swap-backup")
    }

    pub fn claude_config_home(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".claude"))
    }

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

    pub fn launchctl(&self) -> &FakeLaunchctl {
        &self.launchctl
    }

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

    pub fn sequence(&self) -> Value {
        self.read_json(self.store_dir().join("sequence.json"))
    }

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
            cwd: None,
            arg0: None,
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
            format!(
                "{}:{}:{inherited_path}",
                self.claude.bin_dir.display(),
                self.launchctl.bin_dir.display()
            ),
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
    let on_path = |name: &str| {
        vars.get("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(p).collect::<Vec<_>>())
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    match on_path("launchctl") {
        Some(found) if found.starts_with(root) => {}
        found => return Err(format!("launchctl on PATH is not the fake: {found:?}")),
    }
    match on_path("claude") {
        Some(found) if !found.starts_with(root) => {
            return Err(format!("claude on PATH is not the fake: {found:?}"));
        }
        _ => {}
    }
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
    cwd: Option<PathBuf>,
    arg0: Option<String>,
}

impl Cmd<'_> {
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn using(mut self, which: Impl) -> Self {
        self.which = which;
        self
    }

    /// The upstream shim sets its own `argv[0]`, so this only affects ccswap.
    pub fn arg0(mut self, name: &str) -> Self {
        self.arg0 = Some(name.into());
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
            if let Some(arg0) = &self.arg0 {
                std::os::unix::process::CommandExt::arg0(&mut c, arg0);
            }
            c
        };
        command
            .args(&args)
            .current_dir(self.cwd.as_deref().unwrap_or(&self.env.home))
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

    /// Starts with stdin closed; collect the result with [`Running::finish`].
    pub fn spawn(self) -> Running {
        check_isolation(&self.vars, &self.env.root())
            .unwrap_or_else(|e| panic!("harness refused to launch the binary: {e}"));
        let (program, mut args, mut vars) = target(&self.env.root(), self.which);
        args.extend(self.args);
        vars.extend(self.vars);
        let mut child = Command::new(&program)
            .env_clear()
            .envs(&vars)
            .args(&args)
            .current_dir(self.cwd.as_deref().unwrap_or(&self.env.home))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", program.display()));
        let (tx, lines) = std::sync::mpsc::channel::<String>();
        let stdout = child.stdout.take().unwrap();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().unwrap();
        let stderr = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Running {
            child,
            lines,
            seen: Vec::new(),
            reader: Some(reader),
            stderr: Some(stderr),
        }
    }

    /// Runs on a 100x32 pseudo-terminal and kills the program after `timeout`.
    pub fn run_in_terminal(self, keys: &str, timeout: std::time::Duration) -> Output {
        self.run_in_terminal_on(None, keys, timeout)
    }

    /// Answers the DA1 query and, given `background`, the OSC 11 query, then types `keys` once more was drawn.
    pub fn run_in_terminal_on(
        self,
        background: Option<&str>,
        keys: &str,
        timeout: std::time::Duration,
    ) -> Output {
        use std::os::fd::FromRawFd;
        check_isolation(&self.vars, &self.env.root())
            .unwrap_or_else(|e| panic!("harness refused to launch the binary: {e}"));
        let (program, mut args, mut vars) = target(&self.env.root(), self.which);
        args.extend(self.args);
        vars.extend(self.vars);
        let (mut master, slave) = unsafe {
            let (mut m, mut s) = (0, 0);
            let mut size = libc::winsize {
                ws_row: 32,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let rc = libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            );
            assert_eq!(rc, 0, "openpty");
            (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s))
        };
        let mut child = Command::new(&program)
            .env_clear()
            .envs(&vars)
            .args(&args)
            .current_dir(&self.env.home)
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", program.display()));
        let mut reader = master.try_clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let deadline = std::time::Instant::now() + timeout;
        let mut output = Vec::new();
        let mut typed = false;
        let mut answered = false;
        const OSC11: &[u8] = b"\x1b]11;?\x07";
        const DA1: &[u8] = b"\x1b[c";
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        let code = loop {
            while let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
                output.extend(chunk);
            }
            if !answered && contains(&output, DA1) {
                if let Some(bg) = background.filter(|_| contains(&output, OSC11)) {
                    master
                        .write_all(format!("\x1b]11;{bg}\x07").as_bytes())
                        .unwrap();
                }
                master.write_all(b"\x1b[?1;2c").unwrap();
                answered = true;
            }
            let drawn = output.len() > [OSC11, DA1].concat().len() || !contains(&output, DA1);
            if !typed && !output.is_empty() && drawn {
                thread::sleep(std::time::Duration::from_millis(300));
                master.write_all(keys.as_bytes()).unwrap();
                typed = true;
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break -1;
            }
        };
        while let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
            output.extend(chunk);
        }
        Output {
            code,
            stdout: String::from_utf8_lossy(&output).into_owned(),
            stderr: String::new(),
        }
    }
}

/// [`Impl::UnderTest`] follows `CCSWAP_TEST_UPSTREAM`; the other two pin one side.
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

pub struct Running {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    seen: Vec<String>,
    reader: Option<thread::JoinHandle<()>>,
    stderr: Option<thread::JoinHandle<String>>,
}

impl Running {
    /// Every line read so far is kept for [`finish`](Self::finish).
    pub fn wait_for_line(
        &mut self,
        timeout: std::time::Duration,
        pred: impl Fn(&str) -> bool,
    ) -> Option<String> {
        if let Some(line) = self.seen.iter().find(|l| pred(l)) {
            return Some(line.clone());
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self.lines.recv_timeout(left).ok()?;
            self.seen.push(line.clone());
            if pred(&line) {
                return Some(line);
            }
        }
    }

    pub fn signal(&self, sig: libc::c_int) {
        let rc = unsafe { libc::kill(self.child.id() as libc::pid_t, sig) };
        assert_eq!(rc, 0, "kill({sig})");
    }

    /// Kills the program after `timeout` and reports code -1.
    pub fn finish(mut self, timeout: std::time::Duration) -> Output {
        let deadline = std::time::Instant::now() + timeout;
        let code = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break -1;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.seen.extend(self.lines.try_iter());
        let stderr = self.stderr.take().map(|h| h.join().unwrap_or_default());
        let mut stdout = self.seen.join("\n");
        if !stdout.is_empty() {
            stdout.push('\n');
        }
        Output {
            code,
            stdout,
            stderr: stderr.unwrap_or_default(),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// Hard-linked from one copy per run because macOS scans each new executable on first exec (~0.3 s, serialized), pushing parallel `security` calls past their 5 s timeout.
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

/// Mirrors `security`: exit 44 for a missing item, the secret printed only with `-w`, `-i` commands read from stdin.
#[derive(Clone)]
pub struct FakeSecurity {
    path: PathBuf,
    state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SecurityCall {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
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

    /// Also applies when `command` arrives through `security -i`.
    pub fn fail_command(&self, command: &str, rc: i32) {
        std::fs::write(self.state.join(format!("fail_{command}")), rc.to_string()).unwrap();
    }

    pub fn fail_command_for_service(&self, command: &str, service: &str, rc: i32) {
        std::fs::write(
            self.state.join(format!("fail_{command}@{service}")),
            rc.to_string(),
        )
        .unwrap();
    }

    pub fn fail_command_for_item(&self, command: &str, service: &str, account: &str, rc: i32) {
        std::fs::write(
            self.state
                .join(format!("fail_{command}@{service}@{account}")),
            rc.to_string(),
        )
        .unwrap();
    }

    pub fn observe(&self, paths: &[&Path]) {
        let list: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        std::fs::write(
            self.state.join("observe.json"),
            serde_json::to_string(&list).unwrap(),
        )
        .unwrap();
    }

    /// Holds for at most 4.5 s because callers time out at 5 s.
    pub fn pause_next(&self, command: &str) {
        let _ = std::fs::remove_file(self.state.join("release"));
        std::fs::write(self.state.join(format!("pause_{command}")), "").unwrap();
    }

    /// Returns false on timeout.
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
    /// Includes any query string.
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

/// Unrouted requests get 404 and are still recorded.
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

    /// Matches `path` without its query string.
    pub fn respond(&self, method: &str, path: &str, response: StubResponse) {
        self.respond_with(method, path, move |_| response.clone());
    }

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

/// Models launchd: `bootstrap` loads the job, `bootout` unloads it, `print` answers only while loaded.
pub struct FakeLaunchctl {
    bin_dir: PathBuf,
    state: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LaunchctlCall {
    pub argv: Vec<String>,
    pub loaded: bool,
}

impl FakeLaunchctl {
    fn install(dir: &Path) -> Self {
        install_script(dir, "launchctl", FAKE_LAUNCHCTL);
        FakeLaunchctl {
            bin_dir: dir.join("bin"),
            state: dir.join("state"),
        }
    }

    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    /// `print` answers with `print_stdout` until a `bootout`.
    pub fn load(&self, print_stdout: &str) {
        std::fs::write(self.state.join("loaded"), print_stdout).unwrap();
    }

    pub fn is_loaded(&self) -> bool {
        self.state.join("loaded").exists()
    }

    /// Answer the next calls of `sub` with `(exit, stdout, stderr)` in order,
    /// then fall back to the launchd model.
    pub fn script(&self, sub: &str, replies: &[(i32, &str, &str)]) {
        let path = self.state.join("script.json");
        let mut script: serde_json::Map<String, Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let replies: Vec<Value> = replies
            .iter()
            .map(|(exit, stdout, stderr)| {
                serde_json::json!({"exit": exit, "stdout": stdout, "stderr": stderr})
            })
            .collect();
        script.insert(sub.into(), Value::Array(replies));
        std::fs::write(&path, Value::Object(script).to_string()).unwrap();
    }

    pub fn calls(&self) -> Vec<LaunchctlCall> {
        read_jsonl(&self.state.join("calls.jsonl"))
            .into_iter()
            .map(|c| LaunchctlCall {
                argv: serde_json::from_value(c["argv"].clone()).unwrap(),
                loaded: c["loaded"].as_bool().unwrap(),
            })
            .collect()
    }

    pub fn subcommands(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|c| c.argv.first().cloned().unwrap_or_default())
            .collect()
    }
}
