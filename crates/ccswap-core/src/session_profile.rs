//! Per-account session profile directories (`<backup>/sessions/<num>-<slug>`)
//! as account management sees them: where they live, and dropping their
//! credential material or the whole profile.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, ErrorKind, Result};
use crate::log;
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
        if !mark_stale(&session_dir) {
            log::error(format!(
                "Account {num}'s backup credentials changed but its live session profile could not be marked stale; it may keep serving the superseded generation once it exits."
            ));
        }
        return;
    }
    invalidate_slot_credentials(keychain, paths, num, email);
    if session_dir.join(".credentials.json").exists() {
        if mark_stale(&session_dir) {
            log::warning(format!(
                "Stored account {num}'s credential but could not invalidate its session profile; marked it stale so the next run re-bootstraps."
            ));
        } else {
            log::error(format!(
                "Stored account {num}'s credential but could NOT invalidate its session profile OR mark it stale; the profile may keep serving the superseded generation until its token expires."
            ));
        }
    }
}

/// `invalidate_credentials` for a slot's profile, logged.
pub fn invalidate_slot_credentials(keychain: &Keychain, paths: &Paths, num: &str, email: &str) {
    let session_dir = session_dir_for(paths, num, email);
    if !session_dir.exists() {
        return;
    }
    invalidate_credentials(keychain, &session_dir);
    log::info(format!("Invalidated session credentials for account {num}"));
}

/// `delete_profile` for a slot's profile, logged; a survivor is reported,
/// not claimed removed.
pub fn delete_slot_profile(keychain: &Keychain, paths: &Paths, num: &str, email: &str) {
    let session_dir = session_dir_for(paths, num, email);
    delete_profile(keychain, &session_dir);
    let marker_left =
        stale_marker_for(&session_dir).exists() || session_dir.join(STALE_MARKER).exists();
    if session_dir.exists() || marker_left {
        log::warning(format!(
            "Could not fully remove account {num}'s session profile at {}; credentials are gone but the profile dir and/or its stale marker survive (check permissions on it and its parent).",
            session_dir.display()
        ));
        return;
    }
    log::info(format!(
        "Removed session profile for account {num} at {}",
        session_dir.display()
    ));
}

/// Best-effort read of the profile's current credential: its hashed
/// Keychain item (which shadows the seed once Claude writes it), else the
/// plaintext `.credentials.json`. Read-only.
pub fn read_credentials(keychain: &Keychain, session_dir: &Path) -> Option<String> {
    if !session_dir.is_dir() {
        return None;
    }
    let service = profile_service_name(&session_dir.display().to_string());
    if let Ok(Some(creds)) = keychain.get_password(&service, keychain.account())
        && !creds.is_empty()
    {
        return Some(creds);
    }
    std::fs::read_to_string(session_dir.join(".credentials.json")).ok()
}

/// `(email, organization uuid)` the profile is logged in as, from its
/// `.claude.json`.
pub fn read_identity(session_dir: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(session_dir.join(".claude.json")).ok()?;
    let config: serde_json::Value = serde_json::from_str(&text).ok()?;
    let account = match config.as_object()?.get("oauthAccount") {
        None | Some(serde_json::Value::Null) => return None,
        Some(v) => v.as_object()?,
    };
    let field = |key: &str| {
        account
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let email = field("emailAddress");
    (!email.is_empty()).then(|| (email, field("organizationUuid")))
}

/// An in-session `/login` can re-point a profile at another account; an
/// unreadable identity is trusted, not drift.
pub fn identity_drifted(session_dir: &Path, email: &str, org_uuid: &str) -> bool {
    let Some((profile_email, profile_org)) = read_identity(session_dir) else {
        return false;
    };
    profile_email != email
        || (!profile_org.is_empty() && !org_uuid.is_empty() && profile_org != org_uuid)
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
    #[test]
    fn identity_drift() {
        let dir = tempfile::tempdir().unwrap();
        let write = |v: &str| std::fs::write(dir.path().join(".claude.json"), v).unwrap();
        assert!(!identity_drifted(dir.path(), "a@x.com", "o"));
        write(r#"{"oauthAccount": {"emailAddress": "b@x.com"}}"#);
        assert!(identity_drifted(dir.path(), "a@x.com", "o"));
        write(r#"{"oauthAccount": {"emailAddress": "a@x.com", "organizationUuid": "p"}}"#);
        assert!(identity_drifted(dir.path(), "a@x.com", "o"));
        assert!(!identity_drifted(dir.path(), "a@x.com", ""));
        write(r#"{"oauthAccount": {"emailAddress": "a@x.com"}}"#);
        assert!(!identity_drifted(dir.path(), "a@x.com", "o"));
        write("not json");
        assert!(!identity_drifted(dir.path(), "a@x.com", "o"));
    }
}
