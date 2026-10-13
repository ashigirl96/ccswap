use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::paths::Paths;

pub type JsonObject = Map<String, Value>;

/// The live login as Claude Code records it in `oauthAccount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub organization_uuid: String,
    pub account_uuid: String,
}

/// `oauthAccount` of the global config; `None` without a usable email.
pub fn current_identity(paths: &Paths) -> Option<Identity> {
    let config = read_json_lenient(&paths.global_config_path())?;
    let account = config.get("oauthAccount")?.as_object()?;
    let text = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let email = text("emailAddress");
    if email.is_empty() {
        return None;
    }
    Some(Identity {
        email,
        organization_uuid: text("organizationUuid"),
        account_uuid: text("accountUuid"),
    })
}

/// `sequence.json`: `None` only when it does not exist; unreadable or
/// non-object content is an error so callers never rebuild the roster from
/// an empty read.
pub fn read_sequence(paths: &Paths) -> Result<Option<JsonObject>> {
    read_json_strict(&paths.sequence_file())
}

pub fn accounts(data: &JsonObject) -> Option<&JsonObject> {
    data.get("accounts").and_then(Value::as_object)
}

/// The slot key whose `(email, organizationUuid)` matches.
pub fn find_account_slot(data: &JsonObject, email: &str, org_uuid: &str) -> Option<String> {
    accounts(data)?.iter().find_map(|(num, account)| {
        let stored_org = match account.get("organizationUuid") {
            None => Some(""),
            Some(v) => v.as_str(),
        };
        (account.get("email").and_then(Value::as_str) == Some(email)
            && stored_org == Some(org_uuid))
        .then(|| num.clone())
    })
}

/// The store's directories, created (or re-hardened) to 0700.
pub fn setup_directories(paths: &Paths) -> Result<()> {
    for dir in [
        paths.backup_root(),
        paths.configs_dir(),
        paths.credentials_dir(),
    ] {
        fsutil::ensure_private_dir(&dir)
            .map_err(|e| Error::config(format!("Could not create {}: {e}", dir.display())))?;
    }
    Ok(())
}

/// Writes an empty roster unless one exists.
pub fn init_sequence(paths: &Paths) -> Result<()> {
    if paths.sequence_file().exists() {
        return Ok(());
    }
    let data = json!({
        "activeAccountNumber": null,
        "lastUpdated": timestamp_now(),
        "sequence": [],
        "accounts": {},
    });
    fsutil::write_json_pid_tmp(&paths.sequence_file(), &data)
}

pub fn write_sequence(paths: &Paths, data: &JsonObject) -> Result<()> {
    fsutil::write_json_pid_tmp(&paths.sequence_file(), &Value::Object(data.clone()))
}

/// `max(slot) + 1`, or 1 for an empty roster; gaps are never refilled.
pub fn next_account_number(data: Option<&JsonObject>) -> u64 {
    data.and_then(accounts)
        .and_then(|a| a.keys().filter_map(|k| k.parse::<u64>().ok()).max())
        .map_or(1, |max| max + 1)
}

pub fn config_backup_path(paths: &Paths, num: &str, email: &str) -> PathBuf {
    paths
        .configs_dir()
        .join(format!(".claude-config-{num}-{email}.json"))
}

/// The stored `.claude.json` copy, `""` when absent.
pub fn read_account_config(paths: &Paths, num: &str, email: &str) -> String {
    std::fs::read_to_string(config_backup_path(paths, num, email)).unwrap_or_default()
}

pub fn write_account_config(paths: &Paths, num: &str, email: &str, config: &str) -> Result<()> {
    let target = config_backup_path(paths, num, email);
    fsutil::atomic_write_private(&paths.configs_dir(), &target, config.as_bytes())
        .map_err(|e| Error::config(format!("Could not write {}: {e}", target.display())))
}

pub fn delete_account_config(paths: &Paths, num: &str, email: &str) {
    let _ = std::fs::remove_file(config_backup_path(paths, num, email));
}

/// Back-fill `organizationUuid`/`organizationName` on records that predate
/// them: the live login's org for the active email, else the backup config's.
pub fn migrate_org_fields(paths: &Paths) -> Result<()> {
    let Some(mut data) = read_sequence(paths)? else {
        return Ok(());
    };
    let live = read_json_lenient(&paths.global_config_path())
        .and_then(|c| c.get("oauthAccount").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    let text = |obj: &JsonObject, key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let live_email = text(&live, "emailAddress");
    let mut updated = false;
    let Some(Value::Object(records)) = data.get_mut("accounts") else {
        return Ok(());
    };
    for (num, record) in records.iter_mut() {
        let Some(record) = record.as_object_mut() else {
            continue;
        };
        if record.contains_key("organizationUuid") {
            continue;
        }
        let email = text(record, "email");
        let (org_uuid, org_name) = if !live_email.is_empty() && email == live_email {
            (
                text(&live, "organizationUuid"),
                text(&live, "organizationName"),
            )
        } else {
            let stored = serde_json::from_str::<Value>(&read_account_config(paths, num, &email))
                .ok()
                .and_then(|c| c.get("oauthAccount").and_then(Value::as_object).cloned())
                .unwrap_or_default();
            (
                text(&stored, "organizationUuid"),
                text(&stored, "organizationName"),
            )
        };
        record.insert("organizationUuid".into(), org_uuid.into());
        record.insert("organizationName".into(), org_name.into());
        updated = true;
    }
    if updated {
        data.insert("lastUpdated".into(), timestamp_now().into());
        write_sequence(paths, &data)?;
    }
    Ok(())
}

/// The slot whose alias matches, case-insensitively; an empty alias never matches.
pub fn find_account_by_alias(data: &JsonObject, alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let wanted = alias.to_lowercase();
    accounts(data)?.iter().find_map(|(num, account)| {
        let stored = account
            .get("alias")
            .and_then(Value::as_str)
            .unwrap_or_default();
        (stored.to_lowercase() == wanted).then(|| num.clone())
    })
}

/// Lowercase and validate a proposed alias (`[a-z0-9_.-]+`, not all digits,
/// no leading `-`). Messages echo the raw input.
pub fn normalize_alias(raw: &str) -> Result<String> {
    let invalid = |m: String| Err(Error::new(ErrorKind::ValidationError, m));
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return invalid("alias cannot be empty".into());
    }
    if normalized.chars().all(|c| c.is_ascii_digit()) {
        return invalid(format!(
            "alias '{raw}' cannot be purely numeric (reserved for slot numbers)"
        ));
    }
    if normalized.starts_with('-') {
        return invalid(format!(
            "alias '{raw}' cannot start with '-' (would be read as a command flag)"
        ));
    }
    if !normalized
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
    {
        return invalid(format!(
            "alias '{raw}' may only contain letters, digits, '-', '_', and '.'"
        ));
    }
    Ok(normalized)
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ`, the roster's timestamp format.
pub fn timestamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_timestamp(secs)
}

fn format_timestamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn read_json_strict(path: &Path) -> Result<Option<JsonObject>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::config(format!(
                "{} exists but could not be read ({e}). Fix what is blocking the read, then retry.",
                path.display()
            )));
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(other) => Err(Error::config(format!(
            "{} holds {}, not a JSON object. Repair or move it, then retry.",
            path.display(),
            python_type_name(&other)
        ))),
        Err(e) => Err(Error::config(format!(
            "{} exists but could not be parsed ({e}). Repair or move it, then retry — refusing to overwrite it unread.",
            path.display()
        ))),
    }
}

pub fn read_json_lenient(path: &Path) -> Option<JsonObject> {
    read_json_strict(path).ok().flatten()
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_seconds_with_z() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_timestamp(1_704_067_199), "2023-12-31T23:59:59Z");
    }

    #[test]
    fn next_number_is_one_past_the_highest_slot() {
        assert_eq!(next_account_number(None), 1);
        let data = json!({"accounts": {"1": {}, "3": {}}});
        assert_eq!(next_account_number(data.as_object()), 4);
    }
}
