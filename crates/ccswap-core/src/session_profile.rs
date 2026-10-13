//! Per-account session profile directories (`<backup>/sessions/<num>-<slug>`)
//! as account management sees them: where they live, and dropping their
//! credential material or the whole profile.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::paths::Paths;
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
fn delete_keychain_entry(keychain: &Keychain, session_dir: &Path) {
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
}
