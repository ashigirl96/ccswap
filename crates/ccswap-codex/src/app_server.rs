//! `CODEX_HOME=<home> codex -s read-only -a never app-server`, spoken to over
//! newline-delimited JSON-RPC on stdio. Codex refreshes the home's login itself (and writes
//! it back) before it answers `account/rateLimits/read`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::program::CodexProgram;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a finished app-server gets to exit on its own after stdin closes.
const EXIT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum AppServerError {
    /// Codex could not refresh the login: only a new login brings it back.
    LoginDead(String),
    Timeout,
    /// `codex` could not be started.
    Unavailable(String),
    /// Any other JSON-RPC error, or a reply that is not JSON-RPC.
    Failed(String),
}

/// The `account/rateLimits/read` result, read with `home` as `CODEX_HOME`.
pub fn read_rate_limits(
    program: &CodexProgram,
    home: &Path,
    timeout: Duration,
) -> Result<Value, AppServerError> {
    let mut child = program
        .command()
        .args(["-s", "read-only", "-a", "never", "app-server"])
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so a timeout can take down whatever it spawned too.
        .process_group(0)
        .spawn()
        .map_err(|e| AppServerError::Unavailable(e.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut session = Session {
        stdin: child.stdin.take(),
        replies: rx,
        deadline: Instant::now() + timeout,
    };
    let result = session.converse();
    drop(session);
    reap(&mut child, result.is_ok());
    result
}

struct Session {
    stdin: Option<ChildStdin>,
    replies: Receiver<String>,
    deadline: Instant,
}

impl Session {
    fn converse(&mut self) -> Result<Value, AppServerError> {
        let client_info = json!({ "name": "ccswap", "version": env!("CARGO_PKG_VERSION") });
        self.request(1, "initialize", Some(json!({ "clientInfo": client_info })))?;
        self.send(&json!({ "method": "initialized" }))?;
        self.request(2, "account/rateLimits/read", None)
    }

    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AppServerError> {
        let mut message = json!({ "id": id, "method": method });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message)?;
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            let line = match self.replies.recv_timeout(left) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err(AppServerError::Timeout),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppServerError::Failed("app-server exited".into()));
                }
            };
            let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            // Notifications and server-to-client requests carry a `method`.
            if reply.get("method").is_some() || reply.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                return Err(if login_is_dead(&message) {
                    AppServerError::LoginDead(message)
                } else {
                    AppServerError::Failed(message)
                });
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn send(&mut self, message: &Value) -> Result<(), AppServerError> {
        let stdin = self.stdin.as_mut().expect("stdin open while conversing");
        writeln!(stdin, "{message}")
            .and_then(|()| stdin.flush())
            .map_err(|_| AppServerError::Failed("app-server closed its stdin".into()))
    }
}

/// Codex words a failed refresh as "... refresh token ... sign in again"; a 401 from the
/// backend after that means the same.
fn login_is_dead(message: &str) -> bool {
    let message = message.to_lowercase();
    [
        "refresh token",
        "refresh_token",
        "sign in again",
        "log in again",
        "login again",
        "unauthorized",
        "401",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Closes stdin, gives a finished server a moment to exit, then kills its whole group.
fn reap(child: &mut Child, finished: bool) {
    let pid = child.id() as libc::pid_t;
    let grace_until = Instant::now() + if finished { EXIT_GRACE } else { Duration::ZERO };
    while Instant::now() < grace_until && !exited_unreaped(pid) {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Until `wait` reaps the leader, its pid still names the group.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Whether `pid` has exited, leaving it a zombie so its pid stays reserved.
fn exited_unreaped(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    rc == 0 && unsafe { info.si_pid() } != 0
}
