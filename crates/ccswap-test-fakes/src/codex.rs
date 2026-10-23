//! Fake `codex`. State files under `<state>`:
//! - `calls.jsonl`: one `{argv, codexHome, codexHomeExisted, cwd}` per run.
//! - `responses.json`: `{"<argv joined by spaces>": {stdout, stderr, exit}}`, checked first.
//! - `login.json`: queue of `{auth?, stdout, stderr, exit}`; each `login` pops one and
//!   writes `auth` to `$CODEX_HOME/auth.json` (0600).
//! - `app_server.json`: `{auth?, results: {method: result}, errors: {method: {code, message}}}`;
//!   `app-server` speaks newline-delimited JSON-RPC on stdio, writes `auth` to
//!   `$CODEX_HOME/auth.json` on the first request after `initialize`, and logs each
//!   request to `app_server_requests.jsonl`.

use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{Reply, append_line, pyjson, read_json};

/// Global options that take a value, as `codex -s read-only -a never app-server` passes them.
const VALUE_OPTS: [&str; 6] = [
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-c",
    "--config",
];

pub fn main(state: &Path, args: &[String]) -> Reply {
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().expect("cwd");
    let record = json!({
        "argv": args,
        "codexHome": codex_home.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "codexHomeExisted": codex_home.as_ref().is_some_and(|p| p.is_dir()),
        "cwd": cwd.to_string_lossy(),
    });
    append_line(
        &state.join("calls.jsonl"),
        &pyjson::to_string_compact(&record),
    );
    let responses = read_json(&state.join("responses.json")).unwrap_or_else(|| json!({}));
    if let Some(reply) = responses.get(args.join(" ")) {
        return Reply::from_json(reply);
    }
    match subcommand(args) {
        Some("login") => login(state, codex_home.as_deref()),
        Some("app-server") => app_server(state, codex_home.as_deref()),
        other => failure(&format!("fake codex: nothing scripted for {other:?}")),
    }
}

fn subcommand(args: &[String]) -> Option<&str> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if VALUE_OPTS.contains(&arg.as_str()) {
            rest.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn failure(message: &str) -> Reply {
    Reply {
        exit: 1,
        stdout: String::new(),
        stderr: format!("{message}\n"),
    }
}

fn login(state: &Path, codex_home: Option<&Path>) -> Reply {
    let path = state.join("login.json");
    let Some(mut queue) = read_json(&path) else {
        return failure("fake codex: no login scripted");
    };
    let Some(replies) = queue.as_array_mut().filter(|q| !q.is_empty()) else {
        return failure("fake codex: no login scripted");
    };
    let reply = replies.remove(0);
    std::fs::write(&path, pyjson::to_string_compact(&queue)).expect("write login.json");
    if let Some(auth) = reply["auth"].as_str() {
        let Some(home) = codex_home else {
            return failure("fake codex: login without CODEX_HOME");
        };
        write_auth(home, auth);
    }
    Reply::from_json(&reply)
}

fn app_server(state: &Path, codex_home: Option<&Path>) -> Reply {
    let script = read_json(&state.join("app_server.json")).unwrap_or_else(|| json!({}));
    let mut pending_auth = script["auth"].as_str().map(str::to_string);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).expect("JSON-RPC line");
        append_line(
            &state.join("app_server_requests.jsonl"),
            &pyjson::to_string_compact(&request),
        );
        let method = request["method"].as_str().unwrap_or_default();
        let Some(id) = request.get("id") else {
            continue;
        };
        if method != "initialize"
            && let (Some(auth), Some(home)) = (pending_auth.take(), codex_home)
        {
            write_auth(home, &auth);
        }
        let response = if let Some(error) = script["errors"].get(method) {
            json!({ "id": id, "error": error })
        } else if let Some(result) = script["results"].get(method) {
            json!({ "id": id, "result": result })
        } else if method == "initialize" {
            json!({ "id": id, "result": { "userAgent": "fake-codex" } })
        } else {
            json!({ "id": id, "error": { "code": -32601, "message": format!("fake codex: {method} not scripted") } })
        };
        writeln!(stdout, "{}", pyjson::to_string_compact(&response)).expect("write stdout");
        stdout.flush().ok();
    }
    Reply {
        exit: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}

fn write_auth(home: &Path, auth: &str) {
    std::fs::create_dir_all(home).expect("create CODEX_HOME");
    let path = home.join("auth.json");
    std::fs::write(&path, auth).expect("write auth.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}
