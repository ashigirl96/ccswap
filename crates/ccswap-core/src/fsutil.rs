//! Upstream's roster and credential-file write recipes, kept distinct
//! because their temp-file names are observable on disk. The state-file
//! writer is `settings::atomic_write_json`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::pyjson;

pub fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// `mkdir -p` then `chmod 0700`, re-applied to an existing directory.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    chmod(dir, 0o700)
}

/// The roster writer (`switcher._write_json`): `<stem>.<pid>.tmp` beside the
/// target, validated, chmod 0600, renamed over it.
pub fn write_json_pid_tmp(path: &Path, data: &Value) -> Result<()> {
    let content = pyjson::to_string_pretty(data);
    let temp = pid_temp_path(path);
    let io = |e: std::io::Error| Error::config(format!("Could not write {}: {e}", path.display()));
    std::fs::write(&temp, &content).map_err(io)?;
    if serde_json::from_str::<Value>(&content).is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::config("Generated invalid JSON"));
    }
    chmod(&temp, 0o600).map_err(io)?;
    std::fs::rename(&temp, path).map_err(io)
}

fn pid_temp_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{stem}.{}.tmp", std::process::id()))
}

/// The credential-file writer: random `.tmp` in `dir`, renamed over `target`,
/// then chmod 0600.
pub fn atomic_write_private(dir: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_private_in(dir, "", target, bytes)
}

/// Like [`atomic_write_private`] with a temp-name prefix (`.mappings-`).
pub fn write_private_in(
    dir: &Path,
    prefix: &str,
    target: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(if prefix.is_empty() { "tmp" } else { prefix })
        .suffix(".tmp")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(dir)?;
    temp.write_all(bytes)?;
    temp.persist(target).map_err(|e| e.error)?;
    chmod(target, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_writer_uses_a_pid_temp_and_leaves_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sequence.json");
        write_json_pid_tmp(&path, &serde_json::json!({"a": "\u{e9}"})).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"a\": \"\\u00e9\"\n}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
