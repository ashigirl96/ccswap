//! `export` / `import`: accounts carried between machines in a JSON envelope
//! (format version 1). Every human message goes to the `notice` sink (stderr
//! in the CLI) so `export -` keeps stdout pure JSON.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::anthropic_client::credential_fingerprint;
use crate::error::{Error, ErrorKind, Result};
use crate::fsutil;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::paths::Paths;
use crate::pyjson;
use crate::secret_store::{SecretStore, looks_like_api_key};
use crate::session_profile;
use crate::store::{self, JsonObject};
use crate::usage_store::{DeadTokenStrike, UsageStore};

pub const FORMAT_VERSION: i64 = 1;

pub struct Transfer<'a> {
    pub paths: &'a Paths,
    pub secrets: &'a SecretStore,
}

/// One validated envelope entry, ready to write.
struct Incoming {
    email: String,
    exported_num: String,
    org_uuid: String,
    org_name: String,
    uuid: String,
    added: String,
    api_key: bool,
    alias: Option<String>,
    creds_text: String,
    config_text: String,
}

enum Outcome {
    Imported,
    Overwrote(DeadTokenStrike),
    Replaced,
}

fn transfer_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::TransferError, message)
}

impl Transfer<'_> {
    /// The envelope text for `destination == "-"`; otherwise writes the file
    /// and returns `None`.
    pub fn export(
        &self,
        destination: &str,
        account: Option<&str>,
        full: bool,
        notice: &mut dyn FnMut(&str),
    ) -> Result<Option<String>> {
        let data = store::read_sequence_migrated(self.paths)?.unwrap_or_default();
        let accounts = store::accounts(&data)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                transfer_error("no accounts to export — run ccswap --add-account first")
            })?;

        let targets: Vec<String> = match account {
            Some(identifier) => match store::resolve_account_identifier(Some(&data), identifier)? {
                Some(num) if accounts.contains_key(&num) => vec![num],
                _ => return Err(transfer_error(format!("account not found: {identifier}"))),
            },
            None => {
                let mut nums: Vec<String> = accounts.keys().cloned().collect();
                nums.sort_by_key(|n| n.parse::<i64>().unwrap_or(i64::MAX));
                nums
            }
        };

        let live = store::current_identity(self.paths);
        let mut entries: Vec<Value> = Vec::new();
        for num in &targets {
            let record = accounts[num].as_object().cloned().unwrap_or_default();
            let email = text_field(&record, "email");
            let org_uuid = text_field(&record, "organizationUuid");
            let is_active = live
                .as_ref()
                .is_some_and(|i| i.email == email && i.organization_uuid == org_uuid);

            let (creds_text, config_text) = if is_active {
                let creds = self
                    .secrets
                    .read_active()
                    .value
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::CredentialReadError,
                            format!("failed to read live credentials for active account {email}"),
                        )
                    })?;
                let config = std::fs::read_to_string(self.paths.global_config_path())
                    .map_err(|_| Error::config("Claude config file not found"))?;
                (creds, config)
            } else {
                let creds = self.secrets.read_backup(num, &email).value;
                let config = store::read_account_config(self.paths, num, &email);
                if creds.is_empty() || config.is_empty() {
                    if account.is_some() {
                        if creds.is_empty() {
                            return Err(Error::new(
                                ErrorKind::CredentialReadError,
                                format!("no backup credentials found for account {num} ({email})"),
                            ));
                        }
                        return Err(Error::config(format!(
                            "no backup config found for account {num} ({email})"
                        )));
                    }
                    notice(&format!(
                        "Skipping Account-{num} ({email}): no stored credentials/config — re-add with: ccswap --add-account --slot {num}"
                    ));
                    continue;
                }
                (creds, config)
            };

            let label = |what: &str| format!("{what} for {email}");
            let mut config = parse_payload(&config_text, &label("config"))?;
            if !full {
                match config.get("oauthAccount") {
                    Some(Value::Object(oauth)) => {
                        config =
                            Map::from_iter([("oauthAccount".into(), Value::Object(oauth.clone()))]);
                    }
                    _ => {
                        return Err(transfer_error(format!(
                            "{} is missing oauthAccount — cannot export",
                            label("config")
                        )));
                    }
                }
            }

            let api_key = looks_like_api_key(&creds_text);
            let credentials = if api_key {
                Value::String(creds_text.trim().to_string())
            } else {
                let mut creds = parse_payload(&creds_text, &label("credentials"))?;
                if !full && let Some(login) = creds.get("claudeAiOauth").cloned() {
                    creds = Map::from_iter([("claudeAiOauth".into(), login)]);
                }
                Value::Object(creds)
            };

            let mut entry = json!({
                "number": num.parse::<i64>().unwrap_or_default(),
                "email": email,
                "uuid": record.get("uuid").cloned().unwrap_or_else(|| "".into()),
                "organizationUuid": org_uuid,
                "organizationName": truthy_or_empty(record.get("organizationName")),
                "added": record.get("added").cloned().unwrap_or_else(|| "".into()),
                "credentials": credentials,
                "config": Value::Object(config),
            });
            if api_key {
                entry["kind"] = "api_key".into();
            }
            if let Some(alias) = record.get("alias").filter(|a| truthy(a)) {
                entry["alias"] = alias.clone();
            }
            entries.push(entry);
        }

        if entries.is_empty() {
            return Err(transfer_error(
                "no exportable accounts — all managed slots are missing stored credentials/config. Re-add with: ccswap --add-account --slot <number>",
            ));
        }

        let recorded_active = data
            .get("activeAccountNumber")
            .cloned()
            .unwrap_or(Value::Null);
        let active_in_payload = if entries
            .iter()
            .any(|e| same_number(&e["number"], &recorded_active))
        {
            recorded_active
        } else {
            Value::Null
        };

        let envelope = json!({
            "version": FORMAT_VERSION,
            "exportedAt": store::timestamp_now(),
            "exportedFrom": "macos",
            "swapVersion": crate::VERSION,
            "encrypted": false,
            "activeAccountNumber": active_in_payload,
            "accounts": entries,
        });
        let serialized = pyjson::to_string_pretty(&envelope);

        if destination == "-" {
            return Ok(Some(serialized));
        }
        let out_path = expand_user(destination, self.paths.home());
        if out_path.is_dir() {
            return Err(transfer_error(format!(
                "export destination must be a file path, not a directory: {}",
                display_path(&out_path)
            )));
        }
        let parent = match out_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        fsutil::write_private_in(&parent, "", &out_path, format!("{serialized}\n").as_bytes())
            .map_err(|e| {
                transfer_error(format!("could not write {}: {e}", display_path(&out_path)))
            })?;
        notice(&format!(
            "Exported {} account(s) to {}",
            entries.len(),
            display_path(&out_path)
        ));
        Ok(None)
    }

    /// `read_stdin` supplies the envelope for `source == "-"`.
    pub fn import(
        &self,
        source: &str,
        force: bool,
        read_stdin: &mut dyn FnMut() -> std::io::Result<String>,
        notice: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let text = if source == "-" {
            read_stdin().map_err(|e| transfer_error(format!("could not read stdin: {e}")))?
        } else {
            let path = expand_user(source, self.paths.home());
            if !path.exists() {
                return Err(transfer_error(format!(
                    "import file not found: {}",
                    display_path(&path)
                )));
            }
            std::fs::read_to_string(&path).map_err(|e| {
                transfer_error(format!("could not read {}: {e}", display_path(&path)))
            })?
        };

        let envelope = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            Ok(_) => return Err(transfer_error("export file must be a JSON object")),
            Err(e) => {
                return Err(transfer_error(format!(
                    "export file is not valid JSON: {e}"
                )));
            }
        };
        let version = envelope.get("version").cloned().unwrap_or(Value::Null);
        if !py_eq_int(&version, FORMAT_VERSION) {
            return Err(transfer_error(format!(
                "unsupported export version: {} (expected {FORMAT_VERSION})",
                py_repr(&version)
            )));
        }
        if envelope.get("encrypted") == Some(&Value::Bool(true)) {
            return Err(transfer_error(
                "encrypted exports are not supported in this version — decrypt before piping (e.g. gpg -d backup.gpg | ccswap --import -)",
            ));
        }
        let raw_accounts = match envelope.get("accounts") {
            Some(Value::Array(list)) if !list.is_empty() => list,
            _ => return Err(transfer_error("export file has no accounts to import")),
        };

        let local = store::read_sequence_migrated(self.paths)?.unwrap_or_default();
        let incoming = validate_all(raw_accounts, &local, notice)?;

        let _lock = StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)?;
        store::setup_directories(self.paths)?;
        store::init_sequence(self.paths)?;

        let envelope_active = match envelope.get("activeAccountNumber") {
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(n.to_string()),
            _ => None,
        };
        let usage = UsageStore::new(&self.paths.usage_cache_dir());
        let (mut imported, mut overwritten, mut skipped, mut replaced) = (0, 0, 0, 0);
        let mut written: HashSet<String> = HashSet::new();
        let mut resolved_active: Option<String> = None;

        for entry in &incoming {
            let is_envelope_active =
                envelope_active.as_deref() == Some(entry.exported_num.as_str());
            let mut data = store::read_sequence_migrated(self.paths)?.unwrap_or_else(empty_roster);
            let identity = (entry.email.clone(), entry.org_uuid.clone());

            let (target, outcome) = match store::find_account_slot(
                &data,
                &entry.email,
                &entry.org_uuid,
            ) {
                Some(existing) => {
                    let outcome = if force {
                        Outcome::Overwrote(usage.dead_token_strike(&existing, &identity))
                    } else if self.slot_token_dead(&existing, &entry.email, &data) {
                        Outcome::Replaced
                    } else {
                        notice(&format!(
                            "Skipped {} (already exists, use --force)",
                            entry.email
                        ));
                        skipped += 1;
                        if is_envelope_active {
                            resolved_active = Some(existing);
                        }
                        continue;
                    };
                    let live =
                        session_profile::live_session_pids(self.paths, &existing, &entry.email);
                    if !live.is_empty() {
                        notice(&format!(
                            "Warning: {} (slot {existing}) has a live session-mode instance (PID {}); its session profile keeps the pre-import credentials until it is restarted via 'ccswap run'.",
                            entry.email,
                            session_profile::join_pids(&live)
                        ));
                    }
                    (existing, outcome)
                }
                None => {
                    let free = !store::accounts(&data)
                        .is_some_and(|a| a.contains_key(&entry.exported_num));
                    let target = if free {
                        entry.exported_num.clone()
                    } else {
                        store::next_account_number(Some(&data)).to_string()
                    };
                    (target, Outcome::Imported)
                }
            };

            self.secrets
                .write_backup(&target, &entry.email, &entry.creds_text)?;
            session_profile::after_backup_write(
                self.secrets.keychain(),
                self.paths,
                &target,
                &entry.email,
            );
            store::write_account_config(self.paths, &target, &entry.email, &entry.config_text)?;
            usage.clear_dead_token(&[(target.clone(), identity)])?;

            let mut record = json!({
                "email": entry.email,
                "uuid": entry.uuid,
                "organizationUuid": entry.org_uuid,
                "organizationName": entry.org_name,
                "added": entry.added,
            });
            if entry.api_key {
                record["kind"] = "api_key".into();
            }
            if let Some(alias) = &entry.alias {
                record["alias"] = alias.clone().into();
            }
            accounts_mut(&mut data).insert(target.clone(), record);
            let num: i64 = target.parse().unwrap_or_default();
            let sequence = sequence_mut(&mut data);
            if !sequence.iter().any(|v| v.as_i64() == Some(num)) {
                sequence.push(num.into());
                sequence.sort_by_key(|v| v.as_i64().unwrap_or_default());
            }
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, &data)?;

            if is_envelope_active {
                resolved_active = Some(target.clone());
            }
            written.insert(target.clone());

            match outcome {
                Outcome::Overwrote(strike) => {
                    notice(&format!("Overwrote {} (slot {target})", entry.email));
                    if strike.strikes > 0 {
                        notice("  └ cleared this slot's stored dead-token strike");
                        let same_generation = strike.fingerprint.as_ref().is_some_and(|struck| {
                            credential_fingerprint(&entry.creds_text).as_deref() == struck.as_str()
                        });
                        if same_generation {
                            notice(
                                "  └ this import holds the same credential generation the strike condemned; another permanent auth failure will quarantine it again — recover with a newer export or a re-login",
                            );
                        }
                    }
                    overwritten += 1;
                }
                Outcome::Replaced => {
                    notice(&format!(
                        "Replaced {} (slot {target} was quarantined: refresh token dead)",
                        entry.email
                    ));
                    replaced += 1;
                }
                Outcome::Imported => {
                    notice(&format!("Imported {} → slot {target}", entry.email));
                    imported += 1;
                }
            }
        }

        let mut fin = store::read_sequence(self.paths)?;
        if let (Some(data), Some(slot)) = (fin.as_mut(), &resolved_active)
            && unset_active(data.get("activeAccountNumber"))
        {
            data.insert(
                "activeAccountNumber".into(),
                slot.parse::<i64>().unwrap_or_default().into(),
            );
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, data)?;
        }

        let mut summary =
            format!("Done: {imported} imported, {overwritten} overwritten, {skipped} skipped");
        if replaced > 0 {
            summary.push_str(&format!(", {replaced} replaced (dead token)"));
        }
        notice(&summary);

        if let (Some(identity), Some(data)) = (store::current_identity(self.paths), fin.as_ref())
            && let Some(live_slot) =
                store::find_account_slot(data, &identity.email, &identity.organization_uuid)
            && written.contains(&live_slot)
        {
            notice(&format!(
                "Note: {} is your current live login — activate the imported credentials with: ccswap --switch-to {live_slot} --force",
                identity.email
            ));
        }
        Ok(())
    }

    /// The collectors' quarantine question for one slot: the strike holds
    /// while any stored source (the live credential too, for the active
    /// slot) still carries the struck generation. An unreadable source is
    /// never condemned.
    fn slot_token_dead(&self, num: &str, email: &str, data: &JsonObject) -> bool {
        let org = store::accounts(data)
            .and_then(|a| a.get(num))
            .and_then(Value::as_object)
            .map(|r| text_field(r, "organizationUuid"))
            .unwrap_or_default();
        let strike = UsageStore::new(&self.paths.usage_cache_dir())
            .dead_token_strike(num, &(email.to_string(), org));
        let is_active = store::current_identity(self.paths).is_some_and(|i| {
            store::find_account_slot(data, &i.email, &i.organization_uuid).as_deref() == Some(num)
        });
        let backup = self.secrets.read_backup(num, email);
        if backup.unreadable {
            return false;
        }
        let stored = if is_active {
            match self.secrets.read_active().value {
                Some(value) => value,
                None => return false,
            }
        } else {
            backup.value.clone()
        };
        if strike.token_dead(credential_fingerprint(&stored).as_deref()) {
            return true;
        }
        is_active
            && !backup.value.is_empty()
            && strike.token_dead(credential_fingerprint(&backup.value).as_deref())
    }
}

/// Pass 1: every entry is checked before anything is written, so a bad
/// entry late in the file cannot leave earlier ones half-imported.
fn validate_all(
    raw_accounts: &[Value],
    local: &JsonObject,
    notice: &mut dyn FnMut(&str),
) -> Result<Vec<Incoming>> {
    let mut local_aliases: HashMap<String, (String, String)> = HashMap::new();
    if let Some(accounts) = store::accounts(local) {
        for record in accounts.values().filter_map(Value::as_object) {
            if let Some(alias) = record.get("alias").filter(|a| truthy(a)) {
                local_aliases.insert(
                    alias.as_str().unwrap_or_default().to_lowercase(),
                    (
                        text_field(record, "email"),
                        text_field(record, "organizationUuid"),
                    ),
                );
            }
        }
    }

    let mut incoming = Vec::new();
    let mut seen_keys: HashSet<(String, String)> = HashSet::new();
    let mut seen_aliases: HashSet<String> = HashSet::new();
    for raw in raw_accounts {
        let (account, email, exported_num) = validate_account(raw)?;
        let org_uuid = text_field(account, "organizationUuid");
        let Some(Value::Object(config)) = account.get("config") else {
            return Err(transfer_error(format!(
                "config for {email} must be a JSON object"
            )));
        };
        let creds = account.get("credentials");
        let api_key = account.get("kind").and_then(Value::as_str) == Some("api_key")
            || matches!(creds, Some(Value::String(_)));
        let creds_text = if api_key {
            match creds {
                Some(Value::String(key)) if looks_like_api_key(key) => key.trim().to_string(),
                _ => {
                    return Err(transfer_error(format!(
                        "API-key credentials for {email} must be a raw sk-ant-api… string"
                    )));
                }
            }
        } else {
            match creds {
                Some(obj @ Value::Object(_)) => pyjson::to_string_compact(obj),
                _ => {
                    return Err(transfer_error(format!(
                        "credentials for {email} must be a JSON object"
                    )));
                }
            }
        };
        if !seen_keys.insert((email.clone(), org_uuid.clone())) {
            let org = if org_uuid.is_empty() {
                "personal"
            } else {
                &org_uuid
            };
            return Err(transfer_error(format!(
                "duplicate account in export: {email} (org={org})"
            )));
        }

        let mut alias = None;
        if let Some(raw_alias) = account
            .get("alias")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        {
            let key = store::normalize_alias(raw_alias)
                .map_err(|e| transfer_error(format!("invalid alias for {email}: {e}")))?;
            if !seen_aliases.insert(key.clone()) {
                return Err(transfer_error(format!("duplicate alias in export: {key}")));
            }
            match local_aliases.get(&key) {
                Some(owner) if *owner != (email.clone(), org_uuid.clone()) => notice(&format!(
                    "Warning: alias '{key}' for {email} already used by an existing account, dropping the imported alias"
                )),
                _ => alias = Some(key),
            }
        }

        incoming.push(Incoming {
            exported_num,
            org_name: text_field(account, "organizationName"),
            uuid: text_field(account, "uuid"),
            added: account
                .get("added")
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty())
                .map_or_else(store::timestamp_now, str::to_string),
            api_key,
            alias,
            creds_text,
            config_text: pyjson::to_string_pretty(&Value::Object(config.clone())),
            org_uuid,
            email,
        });
    }
    Ok(incoming)
}

/// Field checks that must pass before the email and slot number are used to
/// build file names (they guard against path traversal).
fn validate_account(raw: &Value) -> Result<(&JsonObject, String, String)> {
    let Value::Object(account) = raw else {
        return Err(transfer_error("account entry must be a JSON object"));
    };
    let email = match account.get("email") {
        Some(Value::String(e)) if store::is_valid_email(e) => e.clone(),
        other => {
            return Err(transfer_error(format!(
                "invalid or missing email in imported account: {}",
                py_repr(other.unwrap_or(&Value::Null))
            )));
        }
    };
    let number = account.get("number").unwrap_or(&Value::Null);
    let num = match number {
        Value::Number(n) if n.as_u64().is_some_and(|v| v >= 1) => n.to_string(),
        _ => {
            return Err(transfer_error(format!(
                "invalid slot number in imported account ({email}): {}",
                py_repr(number)
            )));
        }
    };
    for field in [
        "organizationUuid",
        "organizationName",
        "uuid",
        "added",
        "alias",
    ] {
        match account.get(field) {
            None | Some(Value::Null) | Some(Value::String(_)) => {}
            Some(other) => {
                return Err(transfer_error(format!(
                    "{field} for {email} must be a string, got {}",
                    crate::pyjson::py_type_name(other)
                )));
            }
        }
    }
    if let Some(Value::String(alias)) = account.get("alias") {
        store::normalize_alias(alias)
            .map_err(|e| transfer_error(format!("invalid alias for {email}: {e}")))?;
    }
    Ok((account, email, num))
}

fn parse_payload(text: &str, label: &str) -> Result<JsonObject> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(transfer_error(format!("{label} must be a JSON object"))),
        Err(e) => Err(transfer_error(format!("{label} is not valid JSON: {e}"))),
    }
}

fn empty_roster() -> JsonObject {
    let mut data = JsonObject::new();
    data.insert("activeAccountNumber".into(), Value::Null);
    data.insert("lastUpdated".into(), store::timestamp_now().into());
    data.insert("sequence".into(), json!([]));
    data.insert("accounts".into(), json!({}));
    data
}

fn accounts_mut(data: &mut JsonObject) -> &mut JsonObject {
    if !data.get("accounts").is_some_and(Value::is_object) {
        data.insert("accounts".into(), json!({}));
    }
    data.get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("accounts is an object")
}

fn sequence_mut(data: &mut JsonObject) -> &mut Vec<Value> {
    if !data.get("sequence").is_some_and(Value::is_array) {
        data.insert("sequence".into(), json!([]));
    }
    data.get_mut("sequence")
        .and_then(Value::as_array_mut)
        .expect("sequence is an array")
}

/// `record.get(key, "") or ""` for a field expected to be text.
fn text_field(record: &JsonObject, key: &str) -> String {
    record
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn truthy_or_empty(value: Option<&Value>) -> Value {
    value
        .filter(|v| truthy(v))
        .cloned()
        .unwrap_or_else(|| "".into())
}

/// Python's `active in {exported numbers}` for an int-valued exported number.
fn same_number(exported: &Value, active: &Value) -> bool {
    match (exported.as_f64(), active) {
        (Some(n), Value::Number(a)) => a.as_f64() == Some(n),
        _ => false,
    }
}

/// `value == 1` under Python equality (`1.0` and `True` compare equal).
fn py_eq_int(value: &Value, expected: i64) -> bool {
    match value {
        Value::Number(n) => n.as_f64() == Some(expected as f64),
        Value::Bool(b) => i64::from(*b) == expected,
        _ => false,
    }
}

/// `active in (None, 0)`.
fn unset_active(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::Bool(b)) => !b,
        _ => false,
    }
}

/// Python `repr()` of a decoded JSON value.
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => pyjson::float_repr(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (0x7f..0xa0).contains(&(c as u32)) => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `Path(text).expanduser()`.
fn expand_user(text: &str, home: &Path) -> PathBuf {
    if text == "~" {
        return home.to_path_buf();
    }
    match text.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(text),
    }
}

/// `str(Path(...))`: `.` components and doubled or trailing slashes dropped.
fn display_path(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return ".".into();
    }
    let joined = parts.join("/");
    joined
        .strip_prefix("//")
        .map_or(joined.clone(), |rest| format!("/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_follows_python() {
        assert_eq!(py_repr(&json!("it's")), "\"it's\"");
        assert_eq!(py_repr(&json!("a'b\"c")), "'a\\'b\"c'");
        assert_eq!(
            py_repr(&json!({"k": [1, 2.5, null, true]})),
            "{'k': [1, 2.5, None, True]}"
        );
        assert_eq!(py_repr(&json!("tab\there")), "'tab\\there'");
    }

    #[test]
    fn display_path_normalises_like_pathlib() {
        assert_eq!(display_path(Path::new("./a//b/")), "a/b");
        assert_eq!(display_path(Path::new("/x/./y")), "/x/y");
        assert_eq!(display_path(Path::new(".")), ".");
    }
}
