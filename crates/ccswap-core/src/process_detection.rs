//! Running Claude Code instances, from the `<claude_dir>/sessions/<pid>.json`
//! records Claude writes for itself.

use std::path::Path;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSession {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    pub started_at: i64,
    pub kind: String,
    pub entrypoint: String,
    pub status: Option<String>,
}

pub fn is_pid_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence and permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Live sessions and how many records could not be read. Dead PIDs are
/// skipped silently; an unreadable record is counted, since a guard must not
/// read "could not tell" as "nothing running".
pub fn scan_sessions(claude_dir: &Path) -> (Vec<ClaudeSession>, usize) {
    let Ok(entries) = std::fs::read_dir(claude_dir.join("sessions")) else {
        return (Vec::new(), 0);
    };
    let mut sessions = Vec::new();
    let mut unreadable = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match read_record(&path) {
            Some(Some(session)) => sessions.push(session),
            Some(None) => {}
            None => unreadable += 1,
        }
    }
    (sessions, unreadable)
}

/// `None` when unreadable, `Some(None)` for a dead process.
fn read_record(path: &Path) -> Option<Option<ClaudeSession>> {
    let text = std::fs::read_to_string(path).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let data = data.as_object()?;
    let pid = data.get("pid")?.as_i64()?;
    let pid = i32::try_from(pid).ok()?;
    if !is_pid_alive(pid.into()) {
        return Some(None);
    }
    let text = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Some(ClaudeSession {
        pid,
        session_id: text("sessionId"),
        cwd: text("cwd"),
        started_at: data.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
        kind: text("kind"),
        entrypoint: text("entrypoint"),
        status: data
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_record(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(dir.join("sessions").join(name), body).unwrap();
    }

    #[test]
    fn pid_liveness() {
        assert!(is_pid_alive(std::process::id().into()));
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(1));
        assert!(!is_pid_alive(-5));
        assert!(!is_pid_alive(99_999_999));
        assert!(!is_pid_alive(i64::MAX));
    }

    #[test]
    fn missing_sessions_dir_is_empty_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(scan_sessions(dir.path()), (Vec::new(), 0));
    }

    #[test]
    fn dead_pid_is_skipped_not_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "1.json", r#"{"pid": 99999999}"#);
        assert_eq!(scan_sessions(dir.path()), (Vec::new(), 0));
    }

    #[test]
    fn own_pid_is_live_with_fields() {
        let dir = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        write_record(
            dir.path(),
            &format!("{pid}.json"),
            &format!(
                r#"{{"pid": {pid}, "sessionId": "s", "cwd": "/w", "startedAt": 5, "kind": "k", "entrypoint": "cli", "status": "busy"}}"#
            ),
        );
        let (sessions, unreadable) = scan_sessions(dir.path());
        assert_eq!(unreadable, 0);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid as u32, pid);
        assert_eq!(sessions[0].cwd, "/w");
        assert_eq!(sessions[0].status.as_deref(), Some("busy"));
    }

    #[test]
    fn malformed_records_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        for (i, body) in [
            "not json",
            "[1]",
            r#"{"sessionId": "x"}"#,
            r#"{"pid": "12"}"#,
            r#"{"pid": 99999999999999999999}"#,
        ]
        .iter()
        .enumerate()
        {
            write_record(dir.path(), &format!("{i}.json"), body);
        }
        write_record(dir.path(), "notes.txt", "ignored");
        assert_eq!(scan_sessions(dir.path()), (Vec::new(), 5));
    }
}
