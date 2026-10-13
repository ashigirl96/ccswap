//! Per-account session profile directories (`<backup>/sessions/<num>-<slug>`)
//! as account management sees them: where they live, and dropping their
//! credential material or the whole profile.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, ErrorKind, Result};
use crate::paths::Paths;
use crate::process_detection::{self, ClaudeSession};
use crate::secret_store::{Keychain, profile_service_name};

pub const STALE_MARKER: &str = ".cswap-stale-credentials";

/// Filesystem-safe, not injective: the `<num>-` prefix keeps dirs unique.
pub fn slugify_email(email: &str) -> String {
    email
        .nfc()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn session_dir_for(paths: &Paths, num: &str, email: &str) -> PathBuf {
    paths
        .sessions_dir()
        .join(format!("{num}-{}", slugify_email(email)))
}

/// A sibling of the profile dir, so a fault on the dir cannot block it.
pub fn stale_marker_for(session_dir: &Path) -> PathBuf {
    let name = session_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    session_dir.with_file_name(format!(".{name}{STALE_MARKER}"))
}

/// Removes both marker locations (the legacy one is inside the dir);
/// `false` when one survives.
pub fn clear_stale(session_dir: &Path) -> bool {
    [
        stale_marker_for(session_dir),
        session_dir.join(STALE_MARKER),
    ]
    .iter()
    .all(|marker| match std::fs::remove_file(marker) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    })
}

/// The profile's own Keychain item is named after its path, so it must go
/// before the dir does.
pub fn delete_keychain_entry(keychain: &Keychain, session_dir: &Path) {
    let service = profile_service_name(&session_dir.display().to_string());
    let _ = keychain.delete_password(&service, keychain.account());
}

/// Drops the profile's credentials (history kept) so the next `run`
/// re-seeds it from the backup.
pub fn invalidate_credentials(keychain: &Keychain, session_dir: &Path) {
    if !session_dir.exists() {
        return;
    }
    delete_keychain_entry(keychain, session_dir);
    let _ = std::fs::remove_file(session_dir.join(".credentials.json"));
    clear_stale(session_dir);
}

pub fn delete_profile(keychain: &Keychain, session_dir: &Path) {
    if session_dir.exists() {
        delete_keychain_entry(keychain, session_dir);
        let _ = std::fs::remove_dir_all(session_dir);
    }
    clear_stale(session_dir);
}

/// Reads the legacy child location too: a marker left there by an older
/// cswap still owes the profile a re-bootstrap.
pub fn is_stale(session_dir: &Path) -> bool {
    stale_marker_for(session_dir).exists() || session_dir.join(STALE_MARKER).exists()
}

pub fn mark_stale(session_dir: &Path) -> bool {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(stale_marker_for(session_dir))
        .is_ok()
}

pub fn scan_live_sessions(session_dir: &Path) -> (Vec<ClaudeSession>, usize) {
    if !session_dir.exists() {
        return (Vec::new(), 0);
    }
    process_detection::scan_sessions(session_dir)
}

/// Nothing runs against the profile and every record was readable.
pub fn profile_is_quiescent(session_dir: &Path) -> bool {
    let (sessions, unreadable) = scan_live_sessions(session_dir);
    sessions.is_empty() && unreadable == 0
}

pub fn live_session_pids(paths: &Paths, num: &str, email: &str) -> Vec<i32> {
    scan_live_sessions(&session_dir_for(paths, num, email))
        .0
        .iter()
        .map(|s| s.pid)
        .collect()
}

pub fn join_pids(pids: &[i32]) -> String {
    pids.iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refuses a destructive step while a session-mode Claude runs against the
/// slot's profile, or while its records cannot be read.
pub fn ensure_no_live_session(paths: &Paths, num: &str, email: &str, action: &str) -> Result<()> {
    let session_dir = session_dir_for(paths, num, email);
    let (sessions, unreadable) = scan_live_sessions(&session_dir);
    if !sessions.is_empty() {
        let pids: Vec<i32> = sessions.iter().map(|s| s.pid).collect();
        return Err(Error::new(
            ErrorKind::SessionError,
            format!(
                "Account-{num} ({email}) has a live session-mode Claude instance (PID {}). Exit it first, then retry {action}.",
                join_pids(&pids)
            ),
        ));
    }
    if unreadable > 0 {
        return Err(Error::new(
            ErrorKind::SessionError,
            format!(
                "Account-{num} ({email}) has {unreadable} session record(s) that could not be read, so whether a Claude instance is live cannot be determined. Inspect {} and remove or repair them, then retry {action}.",
                session_dir.join("sessions").display()
            ),
        ));
    }
    Ok(())
}

/// After a backup credential write: a profile seeded from the old
/// credential is dropped so the next `run` re-seeds, unless a live Claude
/// owns it, which only gets the stale marker.
pub fn after_backup_write(keychain: &Keychain, paths: &Paths, num: &str, email: &str) {
    let session_dir = session_dir_for(paths, num, email);
    if !live_session_pids(paths, num, email).is_empty() {
        mark_stale(&session_dir);
        return;
    }
    let credentials = session_dir.join(".credentials.json");
    invalidate_credentials(keychain, &session_dir);
    if credentials.exists() {
        mark_stale(&session_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_keeps_ascii_alnum_and_dot_dash_underscore() {
        assert_eq!(slugify_email("user+tag@x.com"), "user_tag_x.com");
        assert_eq!(slugify_email("cafe\u{301}@x.com"), "caf__x.com");
    }

    #[test]
    fn stale_marker_is_a_dot_sibling() {
        assert_eq!(
            stale_marker_for(Path::new("/b/sessions/2-a_x.com")),
            PathBuf::from("/b/sessions/.2-a_x.com.cswap-stale-credentials")
        );
    }
}
