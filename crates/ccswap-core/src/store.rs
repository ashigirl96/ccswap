use std::path::Path;

use serde_json::{Map, Value};

use crate::error::{Error, Result};
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
