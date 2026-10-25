//! Claude Code's inference destination is the default profile's settings env.

use std::path::Path;
use std::time::Duration;

use ccswap_core::{
    error::{Error, Result},
    http_client,
    paths::Paths,
    settings::atomic_write_json,
};
use serde_json::{Map, Value, json};

use crate::openai::{PROXY_BASE_URL, model_map};

pub fn is_enabled(paths: &Paths) -> Result<bool> {
    Ok(settings_use_proxy(&read_settings(
        &paths.default_claude_config_home().join("settings.json"),
    )?))
}

fn settings_use_proxy(settings: &Value) -> bool {
    settings
        .get("env")
        .and_then(|env| env.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        == Some(PROXY_BASE_URL)
}

/// Check the profile actually being launched, after sharing has been synchronized.
pub fn check_launch(paths: &Paths) -> Result<()> {
    // Leave unrelated profile-setting errors to Claude Code as before.
    if read_settings(&paths.claude_config_home().join("settings.json"))
        .is_ok_and(|settings| settings_use_proxy(&settings))
    {
        require_proxy(paths)?;
    }
    Ok(())
}

fn require_proxy(paths: &Paths) -> Result<String> {
    let unavailable = || {
        Error::config(
            "Start ccswap menubar before using OpenAI mode, or run ccswap claude openai off",
        )
    };
    let token = std::fs::read_to_string(paths.backup_root().join("openai/proxy-token"))
        .map_err(|_| unavailable())?;
    let response = http_client::agent(Duration::from_secs(2))
        .get(format!("{PROXY_BASE_URL}/health"))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .map_err(|_| unavailable())?;
    if response.status().as_u16() != 200 {
        return Err(unavailable());
    }
    Ok(token)
}

pub fn enable(paths: &Paths) -> Result<()> {
    let token = require_proxy(paths)?;
    let logins = ccswap_claude::openai::InferenceLogins::new(paths);
    let _lock = logins.lock()?;
    let mut settings = read_settings(&paths.default_claude_config_home().join("settings.json"))?;
    let env = settings
        .as_object_mut()
        .unwrap()
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| Error::config("Claude settings env must be an object"))?;
    let expected = values(Some(token));
    let conflicts: Vec<_> = expected
        .iter()
        .filter(|(key, value)| env.get(*key).is_some_and(|current| current != *value))
        .map(|(key, _)| key.as_str())
        .collect();
    if !conflicts.is_empty() {
        return Err(Error::config(format!(
            "Conflicting Claude settings env keys: {}",
            conflicts.join(", ")
        )));
    }
    env.extend(expected);
    atomic_write_json(
        &paths.default_claude_config_home().join("settings.json"),
        &settings,
    )
}

pub fn disable(paths: &Paths) -> Result<()> {
    let path = paths.default_claude_config_home().join("settings.json");
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    let logins = ccswap_claude::openai::InferenceLogins::new(paths);
    let _lock = logins.lock()?;
    let mut settings = read_settings(&path)?;
    let before = settings.clone();
    let Some(env) = settings.get_mut("env") else {
        return Ok(());
    };
    let env = env
        .as_object_mut()
        .ok_or_else(|| Error::config("Claude settings env must be an object"))?;
    let token = match std::fs::read_to_string(paths.backup_root().join("openai/proxy-token")) {
        Ok(token) => Some(token),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(Error::config(format!(
                "Could not read OpenAI proxy token: {e}"
            )));
        }
    };
    for (key, value) in values(token) {
        if env.get(&key) == Some(&value) {
            env.shift_remove(&key);
        }
    }
    if settings != before {
        atomic_write_json(&path, &settings)?;
    }
    Ok(())
}

fn read_settings(path: &Path) -> Result<Value> {
    let settings = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text)
            .map_err(|e| Error::config(format!("Invalid Claude settings: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => {
            return Err(Error::config(format!(
                "Could not read Claude settings: {e}"
            )));
        }
    };
    if !settings.is_object() {
        return Err(Error::config("Claude settings must be an object"));
    }
    Ok(settings)
}

fn values(token: Option<String>) -> Map<String, Value> {
    let mut values = Map::new();
    values.insert("ANTHROPIC_BASE_URL".into(), PROXY_BASE_URL.into());
    if let Some(token) = token {
        values.insert("ANTHROPIC_AUTH_TOKEN".into(), token.into());
    }
    for (alias, model) in model_map().as_object().expect("embedded model aliases") {
        values.insert(
            format!("ANTHROPIC_DEFAULT_{}_MODEL", alias.to_ascii_uppercase()),
            model.clone(),
        );
    }
    values
}
