//! Safety copies of live credentials a switch displaced without being able
//! to attribute them to the outgoing slot: `credentials/.unclaimed-<id>.enc`
//! (base64, 0600) plus a manifest row of classification evidence. Always
//! files, never the Keychain, so a flaky Keychain cannot start blocking
//! switches through a failed stash.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::paths::Paths;
use crate::settings;
use crate::store::{self, JsonObject};

const MANIFEST: &str = ".unclaimed-manifest.json";
const MANIFEST_LOCK: &str = ".unclaimed-manifest.lock";
const ENTRY_PREFIX: &str = ".unclaimed-";
const ENTRY_SUFFIX: &str = ".enc";

enum ManifestRead {
    Ok(JsonObject),
    Unreadable,
    Corrupt,
}

pub struct UnclaimedStash<'a> {
    paths: &'a Paths,
}

impl<'a> UnclaimedStash<'a> {
    pub fn new(paths: &'a Paths) -> Self {
        UnclaimedStash { paths }
    }

    fn dir(&self) -> PathBuf {
        self.paths.credentials_dir()
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir().join(MANIFEST)
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.dir().join(format!("{ENTRY_PREFIX}{id}{ENTRY_SUFFIX}"))
    }

    /// Stores `credentials` with `context` as its manifest row and returns
    /// the entry id. The bytes land before the row: an entry without a row
    /// is still listed, a row without bytes is worthless.
    pub fn write(&self, credentials: &str, context: JsonObject) -> Result<String> {
        let now = unix_now();
        let created = store::format_timestamp(now);
        let compact = created.replace(['-', ':'], "");
        let digest = hex(&Sha256::digest(credentials.as_bytes()));
        let id = format!(
            "{}-{}-{}",
            compact.trim_end_matches('Z'),
            &digest[..12],
            nonce()?
        );
        fsutil::atomic_write_private(
            &self.dir(),
            &self.entry_path(&id),
            BASE64.encode(credentials.as_bytes()).as_bytes(),
        )
        .map_err(write_error)?;
        let mut row = JsonObject::new();
        row.insert("createdAt".into(), created.into());
        row.extend(context);
        self.mutate_manifest(|entries| {
            entries.insert(id.clone(), Value::Object(row));
        })?;
        Ok(id)
    }

    /// Manifest rows by id, plus entry files that lost their row
    /// (`{"createdAt": null}`).
    pub fn list(&self) -> BTreeMap<String, JsonObject> {
        let mut entries: BTreeMap<String, JsonObject> = match self.read_manifest() {
            ManifestRead::Ok(rows) => rows
                .into_iter()
                .map(|(id, row)| (id, row.as_object().cloned().unwrap_or_default()))
                .collect(),
            ManifestRead::Unreadable | ManifestRead::Corrupt => BTreeMap::new(),
        };
        if let Ok(dir) = std::fs::read_dir(self.dir()) {
            for name in dir
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
            {
                if let Some(id) = name
                    .strip_prefix(ENTRY_PREFIX)
                    .and_then(|rest| rest.strip_suffix(ENTRY_SUFFIX))
                {
                    entries.entry(id.to_string()).or_insert_with(|| {
                        let mut orphan = JsonObject::new();
                        orphan.insert("createdAt".into(), Value::Null);
                        orphan
                    });
                }
            }
        }
        entries
    }

    /// Deletes an entry's bytes and its manifest row.
    pub fn remove(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.entry_path(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
        self.mutate_manifest(|entries| {
            entries.shift_remove(id);
        })
    }

    fn read_manifest(&self) -> ManifestRead {
        let raw = match std::fs::read(self.manifest_path()) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ManifestRead::Ok(JsonObject::new());
            }
            Err(_) => return ManifestRead::Unreadable,
        };
        let parsed = std::str::from_utf8(&raw)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        match parsed
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|m| m.get("entries"))
            .and_then(Value::as_object)
        {
            Some(entries) => ManifestRead::Ok(entries.clone()),
            None => ManifestRead::Corrupt,
        }
    }

    /// Read-modify-write under the manifest's own lock (not the store lock,
    /// which a stash writer may be running without).
    fn mutate_manifest(&self, mutate: impl FnOnce(&mut JsonObject)) -> Result<()> {
        std::fs::create_dir_all(self.dir()).map_err(write_error)?;
        let _lock = StoreLock::acquire(&self.dir().join(MANIFEST_LOCK), STORE_LOCK_TIMEOUT)?;
        let mut entries = match self.read_manifest() {
            ManifestRead::Ok(entries) => entries,
            ManifestRead::Corrupt => JsonObject::new(),
            ManifestRead::Unreadable => {
                return Err(Error::new(
                    ErrorKind::CredentialReadError,
                    "the unclaimed manifest is unreadable; refusing to rewrite it from an empty read, which would orphan every stashed successor it maps",
                ));
            }
        };
        mutate(&mut entries);
        self.set_aside_unparseable_manifest();
        settings::atomic_write_json(
            &self.manifest_path(),
            &json!({"schemaVersion": 1, "entries": entries}),
        )
    }

    /// An unparseable manifest's rows are evidence: rename it aside rather
    /// than overwrite it.
    fn set_aside_unparseable_manifest(&self) {
        let path = self.manifest_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        if serde_json::from_str::<Value>(&text).is_ok() {
            return;
        }
        let aside = path.with_file_name(format!("{MANIFEST}.corrupt-{}", unix_now()));
        let _ = std::fs::rename(&path, aside);
    }
}

fn write_error(e: std::io::Error) -> Error {
    Error::new(ErrorKind::CredentialWriteError, e.to_string())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Keeps ids unique for identical bytes stashed within one second.
fn nonce() -> Result<String> {
    let mut bytes = [0u8; 3];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(write_error)?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::Environment;

    fn paths(home: &std::path::Path) -> Paths {
        Paths::new(&Environment::from_pairs([("HOME", home.to_str().unwrap())]))
    }

    #[test]
    fn a_corrupt_manifest_is_set_aside_on_the_next_write() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        let stash = UnclaimedStash::new(&paths);
        std::fs::create_dir_all(paths.credentials_dir()).unwrap();
        std::fs::write(paths.credentials_dir().join(MANIFEST), "{not json").unwrap();

        let id = stash.write("secret", JsonObject::new()).unwrap();

        let names: Vec<String> = std::fs::read_dir(paths.credentials_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with(".unclaimed-manifest.json.corrupt-")),
            "{names:?}"
        );
        assert_eq!(stash.list().keys().collect::<Vec<_>>(), [&id]);
    }

    #[test]
    fn an_entry_without_a_row_is_listed_as_an_orphan() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::fs::create_dir_all(paths.credentials_dir()).unwrap();
        std::fs::write(paths.credentials_dir().join(".unclaimed-x.enc"), "c2VjcmV0").unwrap();

        let listed = UnclaimedStash::new(&paths).list();

        assert_eq!(
            listed["x"],
            json!({"createdAt": null}).as_object().unwrap().clone()
        );
    }
}
