//! What a session profile shares with the default `~/.claude`: symlinked
//! customizations, opt-in conversation history, and a mirror of the user's
//! `mcpServers`. A manifest records what ccswap created, so removal never
//! touches the user's own data.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use ccswap_core::console::Console;
use ccswap_core::locks::{DirLock, DirLockOptions};
use ccswap_core::log;
use ccswap_core::pyjson;
use ccswap_core::settings::atomic_write_json;
use serde_json::{Map, Value, json};

use super::{RunOptions, Sessions, set_mode};
use crate::session_profile::profile_is_quiescent;

pub const SHARED_ITEMS: [&str; 6] = [
    "settings.json",
    "keybindings.json",
    "CLAUDE.md",
    "skills",
    "commands",
    "agents",
];
pub const HISTORY_ITEMS: [&str; 2] = ["projects", "history.jsonl"];
pub const SHARE_MANIFEST: &str = ".cswap-shared.json";
const MCP_KEY: &str = "mcpServers";
/// The profile's `mcpServers` is (or was) mirrored: gates the one-time
/// stash and `--no-share`'s removal of the key.
pub const MCP_MIRROR_MARKER: &str = ".cswap-mcp-mirror-v1";
pub const MCP_DISPLACED_STASH: &str = ".cswap-mcp-displaced.json";

fn is_known_item(name: &str) -> bool {
    SHARED_ITEMS.contains(&name) || HISTORY_ITEMS.contains(&name)
}

fn is_history(name: &str) -> bool {
    HISTORY_ITEMS.contains(&name)
}

impl Sessions<'_> {
    /// Idempotent; runs on every launch. Always sources the default
    /// `~/.claude`, whatever `CLAUDE_CONFIG_DIR` the caller had.
    pub(super) fn sync_sharing(&self, dir: &Path, options: RunOptions, console: &mut dyn Console) {
        if !dir.is_dir() {
            return;
        }
        self.sync_mcp_servers(dir, options.share, console);
        let active: Vec<&str> = SHARED_ITEMS
            .iter()
            .filter(|_| options.share)
            .chain(HISTORY_ITEMS.iter().filter(|_| options.share_history))
            .copied()
            .collect();
        let source_root = self.paths.default_claude_config_home();
        let manifest_path = dir.join(SHARE_MANIFEST);
        let mut managed = read_manifest(&manifest_path);

        // History is only ever unlinked, never deleted: a stale manifest
        // must not be able to remove real conversation history.
        for name in &managed {
            if !active.contains(&name.as_str()) {
                let dest = dir.join(name);
                if is_history(name) && dest.exists() && !dest.is_symlink() {
                    continue;
                }
                remove_managed(&dest);
            }
        }
        if active.is_empty() {
            let _ = std::fs::remove_file(&manifest_path);
            return;
        }

        let mut new_managed: Vec<String> = Vec::new();
        for name in active {
            let src = source_root.join(name);
            let dest = dir.join(name);
            if is_history(name) && !prepare_history_share(&src, &dest, dir, self, console) {
                continue;
            }
            if !src.exists() {
                if managed.iter().any(|m| m == name) {
                    remove_managed(&dest);
                }
                continue;
            }
            // A link to a link would let Claude's one-hop atomic settings
            // write replace the intermediate link with a regular file.
            let link_target = if src.is_symlink() {
                src.canonicalize().unwrap_or_else(|_| src.clone())
            } else {
                src.clone()
            };

            if dest.is_symlink() {
                if !managed.iter().any(|m| m == name) {
                    managed.push(name.to_string());
                }
                if std::fs::read_link(&dest).is_ok_and(|current| current == link_target) {
                    new_managed.push(name.to_string());
                    continue;
                }
                if std::fs::remove_file(&dest).is_err()
                    || std::os::unix::fs::symlink(&link_target, &dest).is_err()
                {
                    continue;
                }
                new_managed.push(name.to_string());
                continue;
            } else if dest.exists() && !managed.iter().any(|m| m == name) {
                console.line(&self.style.dimmed(&format!(
                    "Not sharing {name}: the session profile already has its own copy."
                )));
                continue;
            }

            if dest.exists() {
                remove_managed(&dest);
            }
            if let Err(e) = std::os::unix::fs::symlink(&link_target, &dest) {
                log::warning(format!("Failed to share {name} into session: {e}"));
                continue;
            }
            new_managed.push(name.to_string());
        }
        write_manifest(&manifest_path, &new_managed);
    }

    /// A pure mirror of the default profile's user-scope `mcpServers`.
    /// Fail-open throughout; the adopted in-sync state takes no lock.
    fn sync_mcp_servers(&self, dir: &Path, share: bool, console: &mut dyn Console) {
        let config_path = dir.join(".claude.json");
        let marker = dir.join(MCP_MIRROR_MARKER);
        let source = if share {
            match self.read_mcp_source() {
                Some(source) => source,
                None => return,
            }
        } else if marker.exists() {
            Map::new()
        } else {
            return;
        };
        if !config_path.exists() {
            return;
        }
        if !is_regular_file(&config_path) {
            log::warning(format!(
                "Not syncing MCP servers: {} is not a regular file.",
                config_path.display()
            ));
            return;
        }
        let Some(existing) = load_json_object(&config_path) else {
            return;
        };
        let Some(target) = mcp_of(&existing) else {
            log::warning(format!(
                "Not syncing MCP servers: the profile's {MCP_KEY} is not an object."
            ));
            return;
        };
        if target == source && (!share || marker.exists()) {
            return;
        }

        let lock_dir = dir.join(".claude.json.lock");
        let _lock = match DirLock::acquire(&lock_dir, DirLockOptions::config()) {
            Ok(lock) => lock,
            Err(e) => {
                log::warning(format!(
                    "Could not sync MCP servers ({e}) — skipping this launch."
                ));
                return;
            }
        };
        let source = if share {
            match self.read_mcp_source() {
                Some(source) => source,
                None => return,
            }
        } else {
            source
        };
        if !is_regular_file(&config_path) {
            return;
        }
        let Some(mut existing) = load_json_object(&config_path) else {
            return;
        };
        let Some(target) = mcp_of(&existing) else {
            return;
        };
        if target == source {
            if share {
                ensure_marker(&marker);
            }
            return;
        }
        if share && !marker.exists() {
            let displaced: Map<String, Value> = target
                .iter()
                .filter(|(name, value)| source.get(*name) != Some(value))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !displaced.is_empty() && !self.stash_displaced_mcp(dir, displaced, console) {
                return;
            }
        }
        if source.is_empty() {
            existing.shift_remove(MCP_KEY);
        } else {
            existing.insert(MCP_KEY.into(), Value::Object(source));
        }
        if let Err(e) = atomic_write_json(&config_path, &Value::Object(existing)) {
            log::warning(format!("Could not sync MCP servers: {e}"));
            return;
        }
        if share {
            ensure_marker(&marker);
        }
    }

    /// `None` when the default config is unusable, `{}` when it simply has
    /// no user servers (which propagates their removal).
    fn read_mcp_source(&self) -> Option<Map<String, Value>> {
        let config = load_json_object(&self.paths.default_global_config_path())?;
        mcp_of(&config)
    }

    /// Write-once; only a valid existing stash counts as a saved copy.
    fn stash_displaced_mcp(
        &self,
        dir: &Path,
        displaced: Map<String, Value>,
        console: &mut dyn Console,
    ) -> bool {
        let stash = dir.join(MCP_DISPLACED_STASH);
        if stash.is_symlink() || stash.exists() {
            if is_valid_stash(&stash) {
                return true;
            }
            log::warning(format!(
                "{MCP_DISPLACED_STASH} exists but is not a valid stash; leaving the profile's MCP servers in place."
            ));
            return false;
        }
        if let Err(e) = atomic_write_json(&stash, &json!({"schemaVersion": 1, MCP_KEY: displaced}))
        {
            log::warning(format!(
                "Could not stash the profile's MCP servers ({e}); leaving them in place."
            ));
            return false;
        }
        console.line(&self.style.dimmed(&format!(
            "Session MCP servers now mirror your default profile; the profile's previous definitions were saved to {MCP_DISPLACED_STASH}."
        )));
        true
    }
}

/// Makes a history item linkable; `false` skips it this launch. Real
/// per-profile history is merged into `~/.claude` first, and only while
/// nothing runs against the profile.
fn prepare_history_share(
    src: &Path,
    dest: &Path,
    dir: &Path,
    sessions: &Sessions<'_>,
    console: &mut dyn Console,
) -> bool {
    let style = sessions.style;
    let name = dest
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if dest.exists() && !dest.is_symlink() {
        if !profile_is_quiescent(dir) {
            console.line(&style.dimmed(&format!(
                "Not sharing {name} yet: another session is using this profile — retrying on the next launch."
            )));
            return false;
        }
        if let Err(e) = merge_history_into_source(src, dest) {
            log::warning(format!(
                "Could not merge {name} into {}: {e}",
                src.display()
            ));
            console.line(&style.dimmed(&format!(
                "Not sharing {name}: merging the profile's existing history failed (see log)."
            )));
            return false;
        }
        console.line(&style.dimmed(&format!(
            "Merged the profile's existing {name} into {} — conversation history is now shared.",
            src.display()
        )));
    }
    if !src.exists() {
        let created = if name.ends_with(".jsonl") {
            src.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| touch_private(src))
        } else {
            mkdir_private(src)
        };
        if let Err(e) = created {
            log::warning(format!("Could not create {}: {e}", src.display()));
            return false;
        }
    }
    true
}

/// Directories merge file by file (first writer wins); `history.jsonl`
/// appends the lines it does not already have.
fn merge_history_into_source(src: &Path, dest: &Path) -> std::io::Result<()> {
    if dest.is_dir() {
        mkdir_private(src)?;
        let mut entries = Vec::new();
        collect_tree(dest, &mut entries)?;
        entries.sort_by(|a, b| components(b).cmp(&components(a)));
        for path in entries {
            let rel = path.strip_prefix(dest).unwrap_or(&path);
            let target = src.join(rel);
            if path.is_dir() {
                std::fs::remove_dir(&path)?;
                continue;
            }
            if target.exists() {
                std::fs::remove_file(&path)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                mkdir_private(parent)?;
            }
            std::fs::rename(&path, &target)?;
        }
        std::fs::remove_dir(dest)
    } else {
        let existing: Vec<String> = if src.exists() {
            splitlines(&std::fs::read_to_string(src)?)
        } else {
            Vec::new()
        };
        let incoming = std::fs::read_to_string(dest)?;
        let lines: Vec<String> = splitlines(&incoming)
            .into_iter()
            .filter(|line| !line.is_empty() && !existing.contains(line))
            .collect();
        if !lines.is_empty() {
            if let Some(parent) = src.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !src.exists() {
                touch_private(src)?;
            }
            let mut file = std::fs::OpenOptions::new().append(true).open(src)?;
            std::io::Write::write_all(&mut file, format!("{}\n", lines.join("\n")).as_bytes())?;
        }
        std::fs::remove_file(dest)
    }
}

fn collect_tree(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_real_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        out.push(path.clone());
        if is_real_dir {
            collect_tree(&path, out)?;
        }
    }
    Ok(())
}

fn components(path: &Path) -> Vec<Component<'_>> {
    path.components().collect()
}

/// Python's `str.splitlines`.
fn splitlines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => lines.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// `mkdir -p` with 0700 on every level it creates, matching Claude Code.
fn mkdir_private(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    for dir in missing.iter().rev() {
        match std::fs::create_dir(dir) {
            Ok(()) => set_mode(dir, 0o700)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn touch_private(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
}

fn read_manifest(path: &Path) -> Vec<String> {
    let Some(data) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let Some(data) = data.as_object() else {
        return Vec::new();
    };
    match data.get("items") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| is_known_item(name))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Atomic, so a concurrent reader never sees a truncated manifest.
fn write_manifest(path: &Path, items: &[String]) {
    let Some(dir) = path.parent() else {
        return;
    };
    let payload = pyjson::to_string_pretty(&json!({"items": items, "mode": "symlink"}));
    let tmp = dir.join(format!(".cswap-shared-{}.tmp", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, payload.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Removes a ccswap-created entry; callers guarantee it is listed in the
/// manifest or is a symlink.
fn remove_managed(dest: &Path) {
    if dest.is_symlink() || dest.is_file() {
        let _ = std::fs::remove_file(dest);
    } else if dest.is_dir() {
        let _ = std::fs::remove_dir_all(dest);
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn load_json_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// A missing key reads as `{}`; a non-object value is unusable.
fn mcp_of(config: &Map<String, Value>) -> Option<Map<String, Value>> {
    match config.get(MCP_KEY) {
        None => Some(Map::new()),
        Some(Value::Object(map)) => Some(map.clone()),
        Some(_) => None,
    }
}

fn ensure_marker(marker: &Path) {
    if !marker.exists()
        && let Err(e) = std::fs::File::create(marker)
    {
        let name = marker.file_name().unwrap_or_default().to_string_lossy();
        log::warning(format!("Could not write {name}: {e}"));
    }
}

fn is_valid_stash(stash: &Path) -> bool {
    is_regular_file(stash)
        && load_json_object(stash)
            .is_some_and(|data| matches!(data.get(MCP_KEY), Some(Value::Object(_))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd\n"), ["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert!(splitlines("").is_empty());
    }

    #[test]
    fn manifest_reads_only_known_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SHARE_MANIFEST);
        std::fs::write(&path, r#"{"items": ["skills", "../etc", 3, "projects"]}"#).unwrap();
        assert_eq!(read_manifest(&path), ["skills", "projects"]);
        std::fs::write(&path, r#"{"items": "skills"}"#).unwrap();
        assert!(read_manifest(&path).is_empty());
        std::fs::write(&path, "[]").unwrap();
        assert!(read_manifest(&path).is_empty());
    }

    #[test]
    fn manifest_round_trip_is_python_formatted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SHARE_MANIFEST);
        write_manifest(&path, &["skills".to_string()]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"items\": [\n    \"skills\"\n  ],\n  \"mode\": \"symlink\"\n}"
        );
    }
}
