//! `mappings.json`: directory → account identity for `run` auto-resolution.
//! Read leniently (missing or corrupt loads as empty), unlike the roster.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::fsutil;

pub const SCHEMA_VERSION: i64 = 1;

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

    /// Drop every mapping that points at `(email, org_uuid)`; returns how many.
    pub fn prune_account(&self, email: &str, org_uuid: &str) -> std::io::Result<usize> {
        let mut mappings = self.load();
        let before = mappings.len();
        mappings.retain(|_, entry| {
            let entry_org = entry
                .get("organizationUuid")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !(entry.get("email").and_then(Value::as_str) == Some(email) && entry_org == org_uuid)
        });
        let pruned = before - mappings.len();
        if pruned > 0 {
            self.write(mappings)?;
        }
        Ok(pruned)
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
