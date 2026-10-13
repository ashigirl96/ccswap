//! `cache/usage.json` (schemaVersion 2): rows keyed by slot, guarded by the
//! slot's `(email, organizationUuid)` so a reused slot never serves the
//! previous account's row. Writes are read-modify-write under
//! `cache/.usage.lock`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};

pub const SCHEMA_VERSION: i64 = 2;

/// `(email, organizationUuid)`.
pub type Identity = (String, String);

pub struct UsageStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl UsageStore {
    pub fn new(cache_dir: &Path) -> Self {
        UsageStore {
            path: cache_dir.join("usage.json"),
            lock_path: cache_dir.join(".usage.lock"),
        }
    }

    /// Lift the dead-token quarantine after a slot's credential was replaced.
    pub fn clear_dead_token(&self, slots: &[(String, Identity)]) -> Result<()> {
        self.mutate(slots, |row| {
            row.insert("claimId".into(), Value::Null);
            row.insert("claimUntil".into(), json!(0.0));
            row.insert("authDeadStrikes".into(), json!(0));
            row.insert("struckFingerprint".into(), Value::Null);
            row.insert("consecutiveFailures".into(), json!(0));
            row.insert("lastError".into(), Value::Null);
            row.insert("backoffUntil".into(), Value::Null);
        })
    }

    fn mutate(
        &self,
        slots: &[(String, Identity)],
        apply: impl Fn(&mut Map<String, Value>),
    ) -> Result<()> {
        if slots.is_empty() {
            return Ok(());
        }
        let _lock = StoreLock::acquire(&self.lock_path, STORE_LOCK_TIMEOUT)?;
        let mut rows = self.read_rows();
        for (num, identity) in slots {
            let fresh = match rows.get(num) {
                Some(Value::Object(row)) => !matches(row, identity),
                _ => true,
            };
            if fresh {
                rows.insert(
                    num.clone(),
                    json!({"email": identity.0, "organizationUuid": identity.1}),
                );
            }
            if let Some(Value::Object(row)) = rows.get_mut(num) {
                apply(row);
            }
        }
        let data = json!({"schemaVersion": SCHEMA_VERSION, "accounts": Value::Object(rows)});
        fsutil::atomic_write_json(&self.path, &data).map_err(|e| {
            Error::new(
                ErrorKind::ClaudeSwitchError,
                format!("Could not write {}: {e}", self.path.display()),
            )
        })
    }

    fn read_rows(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut raw))
                if raw.get("schemaVersion") == Some(&json!(SCHEMA_VERSION)) =>
            {
                match raw.remove("accounts") {
                    Some(Value::Object(rows)) => rows,
                    _ => Map::new(),
                }
            }
            _ => Map::new(),
        }
    }
}

fn matches(row: &Map<String, Value>, identity: &Identity) -> bool {
    row.get("email").and_then(Value::as_str) == Some(identity.0.as_str())
        && row.get("organizationUuid").map_or(Some(""), Value::as_str) == Some(identity.1.as_str())
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_usage_store.py (clear_dead_token,
    //! identity-guarded rows).
    use super::*;

    #[test]
    fn clearing_resets_strikes_and_keeps_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("usage.json"),
            json!({"schemaVersion": 2, "accounts": {
                "1": {"email": "a@x", "organizationUuid": "", "authDeadStrikes": 3,
                      "lastError": "invalid_grant", "fetchedAt": 5.0},
                "2": {"email": "b@x", "organizationUuid": "", "authDeadStrikes": 2}
            }})
            .to_string(),
        )
        .unwrap();
        UsageStore::new(dir.path())
            .clear_dead_token(&[("1".into(), ("a@x".into(), "".into()))])
            .unwrap();
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("usage.json")).unwrap())
                .unwrap();
        let row = &data["accounts"]["1"];
        assert_eq!(row["authDeadStrikes"], 0);
        assert_eq!(row["lastError"], Value::Null);
        assert_eq!(row["fetchedAt"], 5.0);
        assert_eq!(data["accounts"]["2"]["authDeadStrikes"], 2);
    }

    #[test]
    fn a_row_for_another_identity_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("usage.json"),
            json!({"schemaVersion": 2, "accounts": {
                "1": {"email": "old@x", "organizationUuid": "", "fetchedAt": 5.0}
            }})
            .to_string(),
        )
        .unwrap();
        UsageStore::new(dir.path())
            .clear_dead_token(&[("1".into(), ("new@x".into(), "org".into()))])
            .unwrap();
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("usage.json")).unwrap())
                .unwrap();
        assert_eq!(
            data["accounts"]["1"],
            json!({"email": "new@x", "organizationUuid": "org", "claimId": null, "claimUntil": 0.0,
                   "authDeadStrikes": 0, "struckFingerprint": null, "consecutiveFailures": 0,
                   "lastError": null, "backoffUntil": null})
        );
    }
}
