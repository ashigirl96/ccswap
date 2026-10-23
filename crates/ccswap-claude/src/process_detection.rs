//! Running Claude Code instances (upstream `process_detection.py`), read
//! from the records Claude Code itself keeps: `<config>/sessions/<pid>.json`
//! and `<config>/ide/<port>.lock`.

use std::path::Path;

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSession {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// Epoch milliseconds.
    pub started_at: i64,
    pub kind: String,
    pub entrypoint: String,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdeInstance {
    pub port: i64,
    pub pid: i32,
    pub ide_name: String,
    pub workspace_folders: Vec<String>,
}

pub fn is_pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only probes for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Python `int` semantics for a JSON pid (`true` is 1); `None` for
/// anything `os.kill` would reject, including values beyond `pid_t`.
fn pid_of(v: &Value) -> Option<i32> {
    match v {
        Value::Bool(b) => Some(i32::from(*b)),
        Value::Number(n) => n.as_i64().and_then(|p| i32::try_from(p).ok()),
        _ => None,
    }
}

fn read_object(path: &Path) -> Result<Map<String, Value>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    match serde_json::from_str(&text).map_err(|e| e.to_string())? {
        Value::Object(map) => Ok(map),
        other => Err(format!(
            "'{}' object has no attribute 'get'",
            ccswap_core::pyjson::py_type_name(&other)
        )),
    }
}

fn pid_field(data: &Map<String, Value>) -> Result<i32, String> {
    let value = data.get("pid").ok_or("'pid'")?;
    pid_of(value).ok_or_else(|| format!("invalid pid {value}"))
}

fn text(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn files_with_extension(dir: &Path, extension: &str) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == extension))
        .collect()
}

/// Live sessions, and how many records could not be read. A dead pid is
/// not unreadable: a stale record must not block destructive guards.
pub fn scan_sessions(claude_dir: &Path) -> (Vec<ClaudeSession>, usize) {
    let mut sessions = Vec::new();
    let mut unreadable = 0;
    for path in files_with_extension(&claude_dir.join("sessions"), "json") {
        let read = read_object(&path).and_then(|data| Ok((pid_field(&data)?, data)));
        let (pid, data) = match read {
            Ok(read) => read,
            Err(e) => {
                unreadable += 1;
                ccswap_core::log::debug(format!("Skipping session file {}: {e}", path.display()));
                continue;
            }
        };
        if !is_pid_alive(pid) {
            continue;
        }
        sessions.push(ClaudeSession {
            pid,
            session_id: text(&data, "sessionId"),
            cwd: text(&data, "cwd"),
            started_at: data.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
            kind: text(&data, "kind"),
            entrypoint: text(&data, "entrypoint"),
            status: data
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    (sessions, unreadable)
}

/// Scan view only: cannot tell "none live" from "none readable".
pub fn list_sessions(claude_dir: &Path) -> Vec<ClaudeSession> {
    scan_sessions(claude_dir).0
}

pub fn list_ide_instances(claude_dir: &Path) -> Vec<IdeInstance> {
    files_with_extension(&claude_dir.join("ide"), "lock")
        .into_iter()
        .filter_map(|path| {
            let read = read_object(&path).and_then(|data| Ok((pid_field(&data)?, data)));
            let (pid, data) = match read {
                Ok(read) => read,
                Err(e) => {
                    ccswap_core::log::debug(format!(
                        "Skipping IDE lockfile {}: {e}",
                        path.display()
                    ));
                    return None;
                }
            };
            if !is_pid_alive(pid) {
                return None;
            }
            let port = path.file_stem()?.to_str()?.trim().parse().ok()?;
            Some(IdeInstance {
                port,
                pid,
                ide_name: data
                    .get("ideName")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown IDE")
                    .to_string(),
                workspace_folders: data
                    .get("workspaceFolders")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect()
}

pub fn get_running_instances(claude_dir: &Path) -> (Vec<ClaudeSession>, Vec<IdeInstance>) {
    (list_sessions(claude_dir), list_ide_instances(claude_dir))
}

pub fn entrypoint_label(entrypoint: &str) -> &str {
    match entrypoint {
        "cli" => "CLI",
        "claude-vscode" => "VS Code",
        "claude-desktop" => "Desktop",
        "sdk-cli" | "sdk-ts" | "sdk-py" => "SDK",
        "mcp" => "MCP",
        "local-agent" => "Agent",
        "remote" => "Remote",
        other => other,
    }
}

pub fn ide_short_name(ide_name: &str) -> &str {
    match ide_name {
        "Visual Studio Code" => "VS Code",
        other => other,
    }
}

/// A plain string-prefix replace, as upstream does (`/Users/foobar` also
/// matches a home of `/Users/foo`).
pub fn abbreviate_path(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// One `Running instances:` row: a (label, folder) pair and its counts.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceGroup {
    pub label: String,
    pub cwd: String,
    pub sessions: usize,
    pub ide: usize,
}

pub fn group_instances(
    sessions: &[ClaudeSession],
    ides: &[IdeInstance],
    home: &str,
) -> Vec<InstanceGroup> {
    let mut groups: Vec<InstanceGroup> = Vec::new();
    let mut bump = |label: &str, cwd: String, session: bool| {
        let group = match groups.iter().position(|g| g.label == label && g.cwd == cwd) {
            Some(i) => &mut groups[i],
            None => {
                groups.push(InstanceGroup {
                    label: label.to_string(),
                    cwd,
                    sessions: 0,
                    ide: 0,
                });
                groups.last_mut().expect("just pushed")
            }
        };
        if session {
            group.sessions += 1;
        } else {
            group.ide += 1;
        }
    };
    for s in sessions {
        bump(
            entrypoint_label(&s.entrypoint),
            abbreviate_path(&s.cwd, home),
            true,
        );
    }
    for ide in ides {
        for folder in &ide.workspace_folders {
            bump(
                ide_short_name(&ide.ide_name),
                abbreviate_path(folder, home),
                false,
            );
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_process_detection.py (TestIsPidAlive,
    //! TestListSessions, TestListIdeInstances) and test_printer.py
    //! (entrypoint labels, abbreviate_path).
    use super::*;
    use serde_json::json;

    const DEAD: i32 = 999_999;

    fn me() -> i32 {
        std::process::id() as i32
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn pid_liveness() {
        assert!(is_pid_alive(me()));
        assert!(!is_pid_alive(DEAD));
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(1));
        assert!(!is_pid_alive(-5));
    }

    #[test]
    fn sessions_keep_live_records_and_count_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let live = json!({"pid": me(), "sessionId": "abc", "cwd": "/p", "startedAt": 1_700_000_000_000_i64,
                          "kind": "interactive", "entrypoint": "cli", "status": "busy"});
        write(dir.path(), "sessions/1.json", &live.to_string());
        write(
            dir.path(),
            "sessions/2.json",
            &json!({"pid": DEAD}).to_string(),
        );
        write(dir.path(), "sessions/3.json", "{corrupt");
        write(
            dir.path(),
            "sessions/4.json",
            &json!({"cwd": "/x"}).to_string(),
        );
        write(
            dir.path(),
            "sessions/5.json",
            &json!({"pid": "123"}).to_string(),
        );
        write(dir.path(), "sessions/6.json", "[1]");
        write(
            dir.path(),
            "sessions/7.json",
            &json!({"pid": 1_i64 << 40}).to_string(),
        );
        write(dir.path(), "sessions/readme.txt", "ignored");
        let (sessions, unreadable) = scan_sessions(dir.path());
        assert_eq!(unreadable, 5);
        assert_eq!(
            sessions,
            [ClaudeSession {
                pid: me(),
                session_id: "abc".into(),
                cwd: "/p".into(),
                started_at: 1_700_000_000_000,
                kind: "interactive".into(),
                entrypoint: "cli".into(),
                status: Some("busy".into()),
            }]
        );
    }

    #[test]
    fn sessions_default_optional_fields_and_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(scan_sessions(dir.path()), (vec![], 0));
        write(
            dir.path(),
            "sessions/1.json",
            &json!({"pid": me()}).to_string(),
        );
        let s = &list_sessions(dir.path())[0];
        assert_eq!(
            (s.cwd.as_str(), s.started_at, s.status.as_deref()),
            ("", 0, None)
        );
    }

    #[test]
    fn ide_lockfiles_take_port_from_name_and_skip_bad_records() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list_ide_instances(dir.path()), vec![]);
        write(
            dir.path(),
            "ide/12345.lock",
            &json!({"pid": me(), "ideName": "Cursor", "workspaceFolders": ["/a", "/b"]})
                .to_string(),
        );
        write(dir.path(), "ide/2.lock", &json!({"pid": DEAD}).to_string());
        write(dir.path(), "ide/3.lock", "{bad");
        write(
            dir.path(),
            "ide/4.lock",
            &json!({"ideName": "x"}).to_string(),
        );
        write(
            dir.path(),
            "ide/abc.lock",
            &json!({"pid": me()}).to_string(),
        );
        assert_eq!(
            list_ide_instances(dir.path()),
            [IdeInstance {
                port: 12345,
                pid: me(),
                ide_name: "Cursor".into(),
                workspace_folders: vec!["/a".into(), "/b".into()],
            }]
        );
        write(
            dir.path(),
            "ide/12345.lock",
            &json!({"pid": me()}).to_string(),
        );
        let ide = &list_ide_instances(dir.path())[0];
        assert_eq!(
            (ide.ide_name.as_str(), ide.workspace_folders.len()),
            ("Unknown IDE", 0)
        );
    }

    #[test]
    fn labels_and_home_abbreviation() {
        assert_eq!(entrypoint_label("sdk-py"), "SDK");
        assert_eq!(entrypoint_label("weird"), "weird");
        assert_eq!(ide_short_name("Visual Studio Code"), "VS Code");
        assert_eq!(abbreviate_path("/Users/me/p", "/Users/me"), "~/p");
        assert_eq!(abbreviate_path("/Users/meow", "/Users/me"), "~ow");
        assert_eq!(abbreviate_path("/opt/x", "/Users/me"), "/opt/x");
    }
}
