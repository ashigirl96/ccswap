//! `menubar_settings.json`: the menu bar's display preferences and the
//! auto-switch on/off toggle. Upstream's Python cswap reads the same file.

use std::path::Path;

use ccswap_core::pyjson;
use serde_json::{Map, Value};

pub const FILENAME: &str = "menubar_settings.json";
pub const REFRESH_CHOICES: [i64; 3] = [30, 60, 300];
pub const TITLE_PCT_CHOICES: [&str; 4] = ["off", "5h", "7d", "both"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuBarSettings {
    pub show_account_name: bool,
    /// One of [`TITLE_PCT_CHOICES`]; not validated, as upstream.
    pub title_pct: String,
    pub title_scoped: bool,
    pub refresh_interval: i64,
    pub auto_switch_enabled: bool,
}

impl Default for MenuBarSettings {
    fn default() -> Self {
        MenuBarSettings {
            show_account_name: true,
            title_pct: "both".into(),
            title_scoped: false,
            refresh_interval: 60,
            auto_switch_enabled: false,
        }
    }
}

impl MenuBarSettings {
    /// A missing or unparseable file gives the defaults; a field whose JSON
    /// type differs from its default's Python type keeps the default.
    pub fn load(path: &Path) -> Self {
        let mut settings = Self::default();
        let Some(raw) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return settings;
        };
        let Some(raw) = raw.as_object() else {
            return settings;
        };
        let flag = |key: &str| raw.get(key).and_then(Value::as_bool);
        if let Some(v) = flag("show_account_name") {
            settings.show_account_name = v;
        }
        if let Some(v) = raw.get("title_pct").and_then(Value::as_str) {
            settings.title_pct = v.to_string();
        }
        if let Some(v) = flag("title_scoped") {
            settings.title_scoped = v;
        }
        if let Some(v) = raw.get("refresh_interval").and_then(python_int) {
            settings.refresh_interval = v;
        }
        if let Some(v) = flag("auto_switch_enabled") {
            settings.auto_switch_enabled = v;
        }
        settings
    }

    /// Plain write with the default umask, not atomic: upstream's format.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, pyjson::to_string_pretty(&self.to_json()))
    }

    fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("show_account_name".into(), self.show_account_name.into());
        out.insert("title_pct".into(), self.title_pct.clone().into());
        out.insert("title_scoped".into(), self.title_scoped.into());
        out.insert("refresh_interval".into(), self.refresh_interval.into());
        out.insert(
            "auto_switch_enabled".into(),
            self.auto_switch_enabled.into(),
        );
        Value::Object(out)
    }
}

/// Python `isinstance(x, int)`: `bool` is an `int` subclass, floats are not.
fn python_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_menubar.py (settings).
    use super::*;

    #[test]
    fn settings_defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let s = MenuBarSettings::load(&dir.path().join("nope.json"));
        assert!(s.show_account_name);
        assert_eq!(s.title_pct, "both");
        assert_eq!(s.refresh_interval, 60);
        assert!(!s.auto_switch_enabled);
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(FILENAME);
        let original = MenuBarSettings {
            show_account_name: false,
            title_pct: "5h".into(),
            refresh_interval: 300,
            auto_switch_enabled: true,
            ..Default::default()
        };
        original.save(&path).unwrap();
        assert_eq!(MenuBarSettings::load(&path), original);
    }

    #[test]
    fn settings_file_is_python_json_dumps_indent_2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        MenuBarSettings::default().save(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"show_account_name\": true,\n  \"title_pct\": \"both\",\n  \
             \"title_scoped\": false,\n  \"refresh_interval\": 60,\n  \
             \"auto_switch_enabled\": false\n}"
        );
    }

    #[test]
    fn settings_corrupt_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(MenuBarSettings::load(&path), MenuBarSettings::default());
        std::fs::write(&path, "[1, 2]").unwrap();
        assert_eq!(MenuBarSettings::load(&path), MenuBarSettings::default());
    }

    #[test]
    fn settings_ignores_unknown_and_bad_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        std::fs::write(
            &path,
            r#"{"refresh_interval": "fast", "bogus": 1, "show_account_name": false}"#,
        )
        .unwrap();
        let s = MenuBarSettings::load(&path);
        assert_eq!(s.refresh_interval, 60);
        assert!(!s.show_account_name);
    }

    #[test]
    fn settings_type_checks_follow_python_isinstance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILENAME);
        std::fs::write(
            &path,
            r#"{"refresh_interval": true, "show_account_name": 0, "title_pct": "weird"}"#,
        )
        .unwrap();
        let s = MenuBarSettings::load(&path);
        assert_eq!(s.refresh_interval, 1);
        assert!(s.show_account_name);
        assert_eq!(s.title_pct, "weird");
        std::fs::write(&path, r#"{"refresh_interval": 30.0}"#).unwrap();
        assert_eq!(MenuBarSettings::load(&path).refresh_interval, 60);
    }
}
