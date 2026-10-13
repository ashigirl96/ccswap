//! `mappings.json`: directory → account identity for `run` auto-resolution.
//! Read leniently (missing or corrupt loads as empty), unlike the roster.
//! Identity is (email, organizationUuid), never the slot number, because
//! slot numbers are reused after a remove.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::accounts::display_tag;
use crate::env::Environment;
use crate::error::Result;
use crate::fsutil;
use crate::paths::Paths;
use crate::store;

pub const SCHEMA_VERSION: i64 = 1;

const MAX_SYMLINK_HOPS: usize = 40;

pub struct MappingStore {
    path: PathBuf,
}

impl MappingStore {
    pub fn new(backup_root: &Path) -> Self {
        MappingStore {
            path: backup_root.join("mappings.json"),
        }
    }

    pub fn load(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut data)) => match data.remove("mappings") {
                Some(Value::Object(mappings)) => mappings,
                _ => Map::new(),
            },
            _ => Map::new(),
        }
    }

    /// Exact-match lookup of an already normalized key (no ancestor walk).
    pub fn get(&self, key: &str) -> Option<Value> {
        self.load().remove(key)
    }

    /// Upserts the mapping for a normalized key.
    pub fn set(&self, key: &str, email: &str, org_uuid: &str) -> std::io::Result<()> {
        let mut mappings = self.load();
        mappings.insert(
            key.to_string(),
            json!({"email": email, "organizationUuid": org_uuid, "added": store::timestamp_now()}),
        );
        self.write(mappings)
    }

    /// Whether a mapping for the normalized key existed (and was removed).
    pub fn remove(&self, key: &str) -> std::io::Result<bool> {
        let mut mappings = self.load();
        if mappings.shift_remove(key).is_none() {
            return Ok(false);
        }
        self.write(mappings)?;
        Ok(true)
    }

    /// Drop every mapping that points at `(email, org_uuid)`; returns how many.
    pub fn prune_account(&self, email: &str, org_uuid: &str) -> std::io::Result<usize> {
        let mut mappings = self.load();
        let before = mappings.len();
        mappings.retain(|_, entry| {
            let (entry_email, entry_org) = identity(entry);
            !(entry_email == email && entry_org == org_uuid)
        });
        let pruned = before - mappings.len();
        if pruned > 0 {
            self.write(mappings)?;
        }
        Ok(pruned)
    }

    /// The deepest mapping at or above the normalized `target`. Matching is
    /// per path component, so `/a/foo` never covers `/a/foobar`.
    pub fn resolve(&self, target: &str) -> Option<(String, Value)> {
        let target = Path::new(target);
        let mut best: Option<(String, Value)> = None;
        let mut best_len = 0;
        for (key, entry) in self.load() {
            let candidate = Path::new(&key);
            let len = key.chars().count();
            if candidate.is_absolute()
                && target.starts_with(candidate)
                && (best.is_none() || len > best_len)
            {
                best_len = len;
                best = Some((key, entry));
            }
        }
        best
    }

    fn write(&self, mappings: Map<String, Value>) -> std::io::Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("/"));
        fsutil::ensure_private_dir(parent)?;
        let payload = json!({"schemaVersion": SCHEMA_VERSION, "mappings": Value::Object(mappings)});
        fsutil::write_private_in(
            parent,
            ".mappings-",
            &self.path,
            crate::pyjson::to_string_pretty(&payload).as_bytes(),
        )
    }
}

/// `(email, organizationUuid)` of a mapping entry; missing or non-string
/// fields read as "".
fn identity(entry: &Value) -> (&str, &str) {
    let field = |name| entry.get(name).and_then(Value::as_str).unwrap_or_default();
    (field("email"), field("organizationUuid"))
}

/// The mapping key for `path`: `~` expanded, made absolute against the
/// working directory, symlinks resolved as far as the path exists
/// (upstream `Path(p).expanduser().resolve()`, so `/var` → `/private/var`).
pub fn normalize_path(path: &Path, env: &Environment) -> String {
    let expanded = expand_user(path, env);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(expanded)
    };
    realpath(&absolute).to_string_lossy().into_owned()
}

fn expand_user(path: &Path, env: &Environment) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => env.home().join(components.as_path()),
        _ => path.to_path_buf(),
    }
}

/// Python's non-strict `os.path.realpath`: missing components are kept
/// verbatim, `..` applies to the already resolved prefix.
fn realpath(absolute: &Path) -> PathBuf {
    let mut resolved = PathBuf::from("/");
    let mut pending: Vec<OsString> = Vec::new();
    push_components(&mut pending, absolute);
    let mut hops = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        match std::fs::read_link(&next) {
            Ok(target) if hops < MAX_SYMLINK_HOPS => {
                hops += 1;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                push_components(&mut pending, &target);
            }
            _ => resolved = next,
        }
    }
    resolved
}

/// Pushes `path`'s components so that popping yields them in order.
fn push_components(pending: &mut Vec<OsString>, path: &Path) {
    let names: Vec<OsString> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect();
    pending.extend(names.into_iter().rev());
}

/// Where `run` without an account lands for a directory.
#[derive(Debug, PartialEq, Eq)]
pub enum DirectoryAccount {
    Unmapped,
    /// A mapping covers the directory but its account is no longer stored.
    Removed {
        email: String,
    },
    Mapped {
        slot: String,
        email: String,
    },
}

/// Upstream `slot_for_directory`.
pub fn account_for_directory(paths: &Paths, key: &str) -> Result<DirectoryAccount> {
    let Some((_, entry)) = MappingStore::new(&paths.backup_root()).resolve(key) else {
        return Ok(DirectoryAccount::Unmapped);
    };
    let (email, org) = identity(&entry);
    let data = store::read_sequence_migrated(paths)?.unwrap_or_default();
    let email = email.to_string();
    Ok(match store::find_account_slot(&data, &email, org) {
        Some(slot) => DirectoryAccount::Mapped { slot, email },
        None => DirectoryAccount::Removed { email },
    })
}

pub struct MappingRow {
    pub path: String,
    pub email: String,
    /// `(slot, display tag)`, or None when the account was removed.
    pub account: Option<(String, String)>,
}

/// Every mapping sorted by path (upstream `list_mappings`).
pub fn mapping_rows(paths: &Paths) -> Result<Vec<MappingRow>> {
    let mappings = MappingStore::new(&paths.backup_root()).load();
    if mappings.is_empty() {
        return Ok(Vec::new());
    }
    let data = store::read_sequence_migrated(paths)?.unwrap_or_default();
    let mut rows: Vec<MappingRow> = mappings
        .iter()
        .map(|(path, entry)| {
            let (email, org) = identity(entry);
            let account = store::find_account_slot(&data, email, org).map(|slot| {
                let org_name = store::accounts(&data)
                    .and_then(|a| a.get(&slot))
                    .and_then(|r| r.get("organizationName"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tag = display_tag(org_name).to_string();
                (slot, tag)
            });
            MappingRow {
                path: path.clone(),
                email: email.to_string(),
                account,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        env: Environment,
        store: MappingStore,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let env = Environment::from_pairs([("HOME", root.join("home").into_os_string())]);
        let store = MappingStore::new(&root.join("backup"));
        Fixture {
            _tmp: tmp,
            root,
            env,
            store,
        }
    }

    impl Fixture {
        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        fn key(&self, path: &Path) -> String {
            normalize_path(path, &self.env)
        }

        fn set(&self, path: &Path, email: &str, org: &str) {
            self.store.set(&self.key(path), email, org).unwrap();
        }

        fn resolved_email(&self, path: &Path) -> Option<String> {
            self.store
                .resolve(&self.key(path))
                .map(|(_, e)| e["email"].as_str().unwrap().to_string())
        }
    }

    #[test]
    fn set_then_get_exact() {
        let f = fixture();
        let repo = f.dir("work/app");
        f.set(&repo, "work@co.com", "org-1");
        let entry = f.store.get(&f.key(&repo)).unwrap();
        assert_eq!(entry["email"], "work@co.com");
        assert_eq!(entry["organizationUuid"], "org-1");
        assert!(!entry["added"].as_str().unwrap().is_empty());
    }

    #[test]
    fn get_missing_returns_none() {
        let f = fixture();
        assert!(f.store.get(&f.key(&f.root.join("nope"))).is_none());
    }

    #[test]
    fn resolve_exact_dir() {
        let f = fixture();
        let repo = f.dir("repo");
        f.set(&repo, "a@x.com", "");
        let (key, entry) = f.store.resolve(&f.key(&repo)).unwrap();
        assert_eq!(key, repo.display().to_string());
        assert_eq!(entry["email"], "a@x.com");
    }

    #[test]
    fn resolve_nested_subdir_inherits() {
        let f = fixture();
        let repo = f.dir("repo");
        let sub = f.dir("repo/src/deep");
        f.set(&repo, "a@x.com", "");
        assert_eq!(f.resolved_email(&sub).as_deref(), Some("a@x.com"));
    }

    #[test]
    fn resolve_longest_ancestor_wins() {
        let f = fixture();
        let cwd = f.dir("work/client/src");
        f.set(&f.root.join("work"), "outer@x.com", "");
        f.set(&f.root.join("work/client"), "inner@x.com", "");
        assert_eq!(f.resolved_email(&cwd).as_deref(), Some("inner@x.com"));
    }

    #[test]
    fn resolve_sibling_prefix_does_not_match() {
        let f = fixture();
        let mapped = f.dir("foo/bar");
        let sibling = f.dir("foo/barbaz");
        f.set(&mapped, "a@x.com", "");
        assert_eq!(f.resolved_email(&sibling), None);
    }

    #[test]
    fn resolve_unmapped_returns_none() {
        let f = fixture();
        f.set(&f.root.join("a"), "a@x.com", "");
        let other = f.dir("b");
        assert_eq!(f.resolved_email(&other), None);
    }

    #[test]
    fn remove() {
        let f = fixture();
        let repo = f.dir("repo");
        f.set(&repo, "a@x.com", "");
        assert!(f.store.remove(&f.key(&repo)).unwrap());
        assert!(f.store.get(&f.key(&repo)).is_none());
        assert!(!f.store.remove(&f.key(&repo)).unwrap());
    }

    #[test]
    fn set_overwrites_same_key() {
        let f = fixture();
        let repo = f.dir("repo");
        f.set(&repo, "a@x.com", "");
        f.set(&repo, "b@x.com", "org-9");
        let entry = f.store.get(&f.key(&repo)).unwrap();
        assert_eq!(entry["email"], "b@x.com");
        assert_eq!(entry["organizationUuid"], "org-9");
        assert_eq!(f.store.load().len(), 1);
    }

    #[test]
    fn prune_account() {
        let f = fixture();
        let (a, b, c) = (f.dir("a"), f.dir("b"), f.dir("c"));
        f.set(&a, "work@x.com", "org-1");
        f.set(&b, "work@x.com", "org-1");
        f.set(&c, "personal@x.com", "");
        assert_eq!(f.store.prune_account("work@x.com", "org-1").unwrap(), 2);
        assert!(f.store.get(&f.key(&a)).is_none());
        assert!(f.store.get(&f.key(&b)).is_none());
        assert!(f.store.get(&f.key(&c)).is_some());
    }

    #[test]
    fn load_missing_file_is_empty() {
        let f = fixture();
        assert!(f.store.load().is_empty());
        assert!(f.store.resolve(&f.key(&f.root)).is_none());
    }

    #[test]
    fn load_corrupt_file_is_empty() {
        let f = fixture();
        let backup = f.dir("backup");
        std::fs::write(backup.join("mappings.json"), "{ not json").unwrap();
        assert!(f.store.load().is_empty());
    }

    #[test]
    fn normalize_path_expands_and_resolves() {
        let f = fixture();
        let repo = f.dir("repo");
        let a = f.key(&repo);
        assert_eq!(f.key(Path::new(&format!("{}/", repo.display()))), a);
        assert_eq!(f.key(&repo.join(".")), a);
        assert_eq!(f.key(&repo.join("sub/..")), a);
        assert_eq!(a, repo.display().to_string());
    }

    #[test]
    fn normalize_path_follows_symlinks_and_keeps_missing_tails() {
        let f = fixture();
        let real = f.dir("real");
        std::os::unix::fs::symlink(&real, f.root.join("link")).unwrap();
        std::os::unix::fs::symlink("real", f.root.join("rel-link")).unwrap();
        assert_eq!(
            f.key(&f.root.join("link/x/y")),
            format!("{}/x/y", real.display())
        );
        assert_eq!(f.key(&f.root.join("rel-link")), real.display().to_string());
        assert_eq!(f.key(&f.root.join("link/..")), f.root.display().to_string());
    }

    #[test]
    fn normalize_path_expands_tilde() {
        let f = fixture();
        assert_eq!(
            f.key(Path::new("~/proj")),
            f.root.join("home/proj").display().to_string()
        );
    }

    #[test]
    fn persisted_schema() {
        let f = fixture();
        let repo = f.dir("repo");
        f.set(&repo, "a@x.com", "org-1");
        let text = std::fs::read_to_string(f.root.join("backup/mappings.json")).unwrap();
        let data: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(data["schemaVersion"], 1);
        assert!(data["mappings"].get(f.key(&repo)).is_some());
    }
}
