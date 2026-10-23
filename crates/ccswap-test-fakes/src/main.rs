//! One binary linked as `security`, `claude`, `codex` and `launchctl`; the name it
//! is run under picks the fake, and `<bin dir>/../state` holds its state.

mod claude;
mod codex;
mod launchctl;
#[allow(dead_code)]
#[path = "../../ccswap-core/src/pyjson.rs"]
mod pyjson;
mod security;
mod shlex;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Reply {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Reply {
    fn from_json(reply: &serde_json::Value) -> Self {
        let text = |key: &str| reply[key].as_str().unwrap_or_default().to_string();
        Reply {
            exit: reply["exit"].as_i64().unwrap_or(0) as i32,
            stdout: text("stdout"),
            stderr: text("stderr"),
        }
    }
}

fn main() {
    let exe = invoked_path();
    let state = exe
        .parent()
        .and_then(Path::parent)
        .expect("fake lives in <dir>/bin")
        .join("state");
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let reply = std::panic::catch_unwind(|| match name.as_str() {
        "security" => security::main(&state, &args),
        "claude" => claude::main(&state, &args),
        "codex" => codex::main(&state, &args),
        "launchctl" => launchctl::main(&state, &args),
        other => Reply {
            exit: 2,
            stdout: String::new(),
            stderr: format!("ccswap-test-fakes: no fake named {other:?}\n"),
        },
    })
    .unwrap_or_else(|_| std::process::exit(1));
    std::io::stdout().write_all(reply.stdout.as_bytes()).ok();
    std::io::stderr().write_all(reply.stderr.as_bytes()).ok();
    std::process::exit(reply.exit);
}

/// The hard link that was executed, not the shared file it points at.
fn invoked_path() -> PathBuf {
    let exe = std::env::current_exe()
        .ok()
        .or_else(|| std::env::args_os().next().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(OsString::new()));
    if exe.is_absolute() {
        exe
    } else {
        std::env::current_dir().unwrap_or_default().join(exe)
    }
}

pub fn append_line(path: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    file.write_all(format!("{line}\n").as_bytes())
        .unwrap_or_else(|e| panic!("append {}: {e}", path.display()));
}

pub fn read_json(path: &Path) -> Option<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => Some(
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display())),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}
