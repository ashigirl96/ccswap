//! Run-once store migrations, recorded by id in `<backup>/.migrations.json`
//! (shared with upstream `cswap`, so ids it recorded are honoured).

use std::collections::HashMap;

use ccswap_core::fsutil;
use ccswap_core::log;
use ccswap_core::paths::Paths;
use serde_json::{Map, Value, json};

use crate::secret_store::{KeychainError, SecretStore};
use crate::store;

pub const STATE_FILENAME: &str = ".migrations.json";
pub const MACOS_KEYRING_TO_SECURITY: &str = "macos_keyring_to_security";
/// Where the `keyring` library kept backups before the `security` CLI.
pub const LEGACY_KEYRING_SERVICE: &str = "claude-code";

const STATE_VERSION: u64 = 1;

/// What a run produced for stderr; diagnostics go to the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub stderr: Vec<String>,
}

enum Outcome {
    Completed,
    Skipped,
}

type Migration = fn(&Paths, &SecretStore, &mut MigrationReport) -> Result<Outcome, String>;

const MIGRATIONS: &[(&str, Migration)] = &[(MACOS_KEYRING_TO_SECURITY, macos_keyring_to_security)];

/// Runs every migration not yet recorded. Never fails; a migration that does
/// not complete stays unrecorded and is retried next time.
pub fn run_migrations(paths: &Paths, secrets: &SecretStore) -> MigrationReport {
    let mut report = MigrationReport::default();
    if !paths.backup_root().exists() {
        return report;
    }
    let applied = load_applied(paths);
    for (id, migration) in MIGRATIONS {
        if applied.contains_key(*id) {
            continue;
        }
        match migration(paths, secrets, &mut report) {
            Ok(Outcome::Completed) => {
                if let Err(e) = mark_applied(paths, id) {
                    log::warning(format!(
                        "Migration {id} ran but recording it failed (will re-run next time): {e}"
                    ));
                }
            }
            Ok(Outcome::Skipped) => {}
            Err(e) => log::warning(format!("Migration {id} did not complete (will retry): {e}")),
        }
    }
    report
}

/// Missing or corrupt state reads as "nothing applied" so it never blocks a migration.
fn load_applied(paths: &Paths) -> Map<String, Value> {
    let state = std::fs::read_to_string(paths.backup_root().join(STATE_FILENAME))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    match state {
        Some(Value::Object(mut map)) => match map.remove("applied") {
            Some(Value::Object(applied)) => applied,
            _ => Map::new(),
        },
        _ => Map::new(),
    }
}

fn mark_applied(paths: &Paths, id: &str) -> std::io::Result<()> {
    let mut applied = load_applied(paths);
    applied.insert(id.into(), store::timestamp_now().into());
    let state = json!({"version": STATE_VERSION, "applied": applied});
    let dir = paths.backup_root();
    fsutil::atomic_write_private(
        &dir,
        &dir.join(STATE_FILENAME),
        ccswap_core::pyjson::to_string_pretty(&state).as_bytes(),
    )
}

/// Copies slot backups from the legacy `claude-code` service to
/// `claude-swap`. Legacy items are read with `security` and left in place:
/// deleting another app's item could prompt, and `purge` removes them.
fn macos_keyring_to_security(
    paths: &Paths,
    secrets: &SecretStore,
    report: &mut MigrationReport,
) -> Result<Outcome, String> {
    let Ok(Some(data)) = store::read_sequence(paths) else {
        return Ok(Outcome::Skipped);
    };
    let accounts = match data.get("accounts") {
        None | Some(Value::Null) => return Ok(Outcome::Completed),
        Some(Value::Object(a)) if a.is_empty() => return Ok(Outcome::Completed),
        Some(Value::Object(a)) => a,
        Some(other) => return Err(format!("accounts is not an object: {other}")),
    };
    let email_of = |info: &Value| {
        info.get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let mut pending: Vec<(&String, String)> = Vec::new();
    for (num, info) in accounts {
        let email = email_of(info);
        let stored = secrets
            .read_backup_keychain(num, &email)
            .map_err(|e| format!("Keychain unavailable, deferring macOS keyring migration: {e}"))?;
        if stored.is_empty() {
            pending.push((num, email));
        }
    }
    if pending.is_empty() {
        return Ok(Outcome::Completed);
    }

    let mut email_counts: HashMap<String, usize> = HashMap::new();
    for info in accounts.values() {
        *email_counts.entry(email_of(info)).or_default() += 1;
    }
    let read_legacy = |username: &str| -> Result<String, KeychainError> {
        secrets
            .keychain()
            .get_password(LEGACY_KEYRING_SERVICE, username)
            .map(Option::unwrap_or_default)
    };
    let warn = |message: String| log::warning(format!("macos_keyring_to_security: {message}"));

    let (mut migrated, mut failed) = (0, 0);
    for (num, email) in pending {
        let canonical = format!("account-{num}-{email}");
        let none_user = format!("account-None-{email}");
        let mut source = canonical.clone();
        let mut creds = match read_legacy(&canonical) {
            Ok(c) => c,
            Err(e) => {
                warn(format!("read of {canonical} failed: {e}"));
                failed += 1;
                continue;
            }
        };
        if creds.is_empty() && num != "None" && email_counts.get(&email) == Some(&1) {
            creds = match read_legacy(&none_user) {
                Ok(c) => c,
                Err(e) => {
                    warn(format!("read of {none_user} failed: {e}"));
                    failed += 1;
                    continue;
                }
            };
            if !creds.is_empty() {
                source = none_user;
            }
        }
        if creds.is_empty() {
            continue;
        }

        let readback = secrets
            .write_backup_keychain(num, &email, &creds)
            .and_then(|()| secrets.read_backup_keychain(num, &email));
        match readback {
            Err(e) => {
                warn(format!("write/read-back for {canonical} failed: {e}"));
                secrets.delete_backup_keychain_quiet(num, &email);
                failed += 1;
                continue;
            }
            Ok(value) if value != creds => {
                warn(format!(
                    "read-back mismatch for {canonical}; discarding the security item and leaving the keyring entry in place"
                ));
                secrets.delete_backup_keychain_quiet(num, &email);
                failed += 1;
                continue;
            }
            Ok(_) => {}
        }
        if secrets
            .keychain()
            .item_exists(LEGACY_KEYRING_SERVICE, &source)
        {
            warn(format!(
                "legacy keyring entry {source} was left behind (delete failed or was denied); harmless — remove manually or via purge"
            ));
        }
        migrated += 1;
    }

    if migrated > 0 {
        report.stderr.push(format!(
            "claude-swap: migrated {migrated} macOS credential(s) from the keyring into the Keychain via security"
        ));
    }
    if failed > 0 {
        return Err(format!(
            "{failed} account(s) could not be migrated to the security service; will retry on next run"
        ));
    }
    Ok(Outcome::Completed)
}
