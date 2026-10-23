//! `<backup_root>/settings.json`: user-tunable preferences shared with
//! upstream. Reads are forgiving (out-of-range values clamp, bad types and a
//! corrupt file fall back to defaults); `config set`/`unset` validate strictly
//! and rewrite only the key they touch, keeping unknown keys and sections.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::paths::Paths;
use crate::pyjson;

pub const SETTINGS_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Float { lo: f64, hi: f64 },
    Int { lo: i64, hi: i64 },
    Bool,
    Choice(&'static [&'static str]),
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(String),
    None,
}

impl SettingValue {
    pub fn to_json(&self) -> Value {
        match self {
            SettingValue::Float(f) => {
                serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
            }
            SettingValue::Int(i) => (*i).into(),
            SettingValue::Bool(b) => (*b).into(),
            SettingValue::Text(s) => s.as_str().into(),
            SettingValue::None => Value::Null,
        }
    }

    /// The way upstream's `format_setting_value` renders it: integral floats
    /// drop the `.0`, `None` is `(none)`.
    pub fn display(&self) -> String {
        match self {
            SettingValue::None => "(none)".into(),
            SettingValue::Bool(b) => b.to_string(),
            SettingValue::Float(f) if f.is_finite() && f.fract() == 0.0 => {
                format!("{}", *f as i128)
            }
            SettingValue::Float(f) => pyjson::float_repr(*f),
            SettingValue::Int(i) => i.to_string(),
            SettingValue::Text(s) => s.clone(),
        }
    }
}

#[derive(Debug)]
pub struct SettingSpec {
    pub section: &'static str,
    pub json_key: &'static str,
    pub kind: Kind,
    pub default: SettingDefault,
    pub help: &'static str,
}

#[derive(Debug)]
pub enum SettingDefault {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(&'static str),
    None,
}

impl SettingSpec {
    pub fn dotted(&self) -> String {
        format!("{}.{}", self.section, self.json_key)
    }

    pub fn default_value(&self) -> SettingValue {
        match self.default {
            SettingDefault::Float(f) => SettingValue::Float(f),
            SettingDefault::Int(i) => SettingValue::Int(i),
            SettingDefault::Bool(b) => SettingValue::Bool(b),
            SettingDefault::Text(s) => SettingValue::Text(s.into()),
            SettingDefault::None => SettingValue::None,
        }
    }

    /// Load-time coercion: clamp numbers, truthiness for bools, defaults for
    /// anything of the wrong type.
    pub fn lenient(&self, raw: Option<&Value>) -> SettingValue {
        let Some(raw) = raw else {
            return self.default_value();
        };
        match self.kind {
            Kind::Float { lo, hi } => match raw.as_f64() {
                Some(v) if raw.is_number() => SettingValue::Float(v.max(lo).min(hi)),
                _ => self.default_value(),
            },
            Kind::Int { lo, hi } => match raw.as_f64() {
                Some(v) if raw.is_number() => {
                    SettingValue::Int(v.max(lo as f64).min(hi as f64).trunc() as i64)
                }
                _ => self.default_value(),
            },
            Kind::Bool => SettingValue::Bool(python_truthy(raw)),
            Kind::Text => match raw.as_str() {
                Some(s) if !s.is_empty() => SettingValue::Text(s.into()),
                _ => self.default_value(),
            },
            Kind::Choice(choices) => match raw.as_str() {
                Some(s) if choices.contains(&s) => SettingValue::Text(s.into()),
                _ => self.default_value(),
            },
        }
    }

    /// Strict parse of a `config set` argument.
    fn parse(&self, raw: &str) -> Result<SettingValue> {
        let dotted = self.dotted();
        match self.kind {
            Kind::Bool => match python_strip(raw).to_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(SettingValue::Bool(true)),
                "false" | "0" | "no" => Ok(SettingValue::Bool(false)),
                _ => Err(Error::config(format!(
                    "{dotted} expects true or false (or 1/0, yes/no), got '{raw}'"
                ))),
            },
            Kind::Choice(choices) => {
                if choices.contains(&raw) {
                    Ok(SettingValue::Text(raw.into()))
                } else {
                    Err(Error::config(format!(
                        "{dotted} must be one of: {}",
                        choices.join(", ")
                    )))
                }
            }
            Kind::Text => {
                let value = python_strip(raw);
                if value.is_empty() {
                    Err(Error::config(format!(
                        "{dotted} expects a non-empty value; use 'ccswap config unset {dotted}' to clear it"
                    )))
                } else {
                    Ok(SettingValue::Text(value.into()))
                }
            }
            Kind::Int { lo, hi } => {
                let value = python_int(raw).ok_or_else(|| {
                    Error::config(format!("{dotted} expects an integer, got '{raw}'"))
                })?;
                if (lo as i128..=hi as i128).contains(&value) {
                    Ok(SettingValue::Int(value as i64))
                } else {
                    Err(self.range_error(lo as f64, hi as f64))
                }
            }
            Kind::Float { lo, hi } => {
                let value = python_float(raw).ok_or_else(|| {
                    Error::config(format!("{dotted} expects a number, got '{raw}'"))
                })?;
                if lo <= value && value <= hi {
                    Ok(SettingValue::Float(value))
                } else {
                    Err(self.range_error(lo, hi))
                }
            }
        }
    }

    fn range_error(&self, lo: f64, hi: f64) -> Error {
        Error::config(format!(
            "{} must be between {} and {}",
            self.dotted(),
            SettingValue::Float(lo).display(),
            SettingValue::Float(hi).display()
        ))
    }
}

/// The `ui` section, which every provider's UI shares.
pub static UI_SETTINGS: &[SettingSpec] = &[SettingSpec {
    section: "ui",
    json_key: "theme",
    kind: Kind::Choice(&["dark", "light", "auto"]),
    default: SettingDefault::Text("auto"),
    help: "Color theme; auto follows the terminal background",
}];

/// The sections settings.json knows, one slice per section; registry order
/// is `config list` order.
#[derive(Debug, Clone, Copy)]
pub struct Registry {
    sections: &'static [&'static [SettingSpec]],
}

impl Registry {
    pub const fn new(sections: &'static [&'static [SettingSpec]]) -> Self {
        Registry { sections }
    }

    pub fn specs(&self) -> impl Iterator<Item = &'static SettingSpec> + use<> {
        self.sections.iter().flat_map(|section| section.iter())
    }

    /// Looks up a dotted key; unknown keys list the valid ones.
    pub fn spec(&self, dotted: &str) -> Result<&'static SettingSpec> {
        self.specs().find(|s| s.dotted() == dotted).ok_or_else(|| {
            let keys: Vec<String> = self.specs().map(SettingSpec::dotted).collect();
            Error::config(format!(
                "unknown setting '{dotted}'\nValid keys: {}",
                keys.join(", ")
            ))
        })
    }

    fn section_names(&self) -> impl Iterator<Item = &'static str> + use<> {
        self.sections
            .iter()
            .filter_map(|section| section.first().map(|spec| spec.section))
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveSetting {
    pub spec: &'static SettingSpec,
    pub value: SettingValue,
    /// Present in the file, even when equal to the default.
    pub is_set: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    path: PathBuf,
    registry: Registry,
}

impl Settings {
    pub fn new(paths: &Paths, registry: Registry) -> Self {
        Settings {
            path: paths.settings_file(),
            registry,
        }
    }

    pub fn in_dir(backup_dir: &Path, registry: Registry) -> Self {
        Settings {
            path: backup_dir.join("settings.json"),
            registry,
        }
    }

    pub fn registry(&self) -> Registry {
        self.registry
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Logs what upstream's lenient reads behind `effective_settings` log:
    /// the raw file, then each section in registry order.
    pub fn effective(&self) -> Vec<EffectiveSetting> {
        let (raw, read_warning) = self.read_lenient_with_warning();
        let warnings = read_warning.iter().cloned().chain(
            self.registry
                .section_names()
                .flat_map(|section| self.choice_load_warnings(section)),
        );
        for warning in warnings {
            crate::log::warning(warning);
        }
        self.registry
            .specs()
            .map(|spec| {
                let section = raw.get(spec.section).and_then(Value::as_object);
                let stored = section.and_then(|s| s.get(spec.json_key));
                EffectiveSetting {
                    spec,
                    value: spec.lenient(stored),
                    is_set: stored.is_some(),
                }
            })
            .collect()
    }

    pub fn get(&self, dotted: &str) -> Result<EffectiveSetting> {
        let spec = self.registry.spec(dotted)?;
        Ok(self
            .effective()
            .into_iter()
            .find(|row| std::ptr::eq(row.spec, spec))
            .expect("every spec has a row"))
    }

    /// Validates and writes one key (plus `schemaVersion`); never
    /// materialises other defaults into the file.
    pub fn set(&self, dotted: &str, raw_value: &str) -> Result<SettingValue> {
        let spec = self.registry.spec(dotted)?;
        let value = spec.parse(raw_value)?;
        let mut raw = self.read_for_write()?;
        stamp_schema_version(&mut raw);
        let mut section = match raw.get(spec.section) {
            Some(Value::Object(section)) => section.clone(),
            _ => Map::new(),
        };
        section.insert(spec.json_key.into(), value.to_json());
        raw.insert(spec.section.into(), Value::Object(section));
        atomic_write_json(&self.path, &Value::Object(raw))?;
        Ok(value)
    }

    /// Removes one key, and its section once empty; `false` (and no write)
    /// when it was not set.
    pub fn unset(&self, dotted: &str) -> Result<bool> {
        let spec = self.registry.spec(dotted)?;
        let mut raw = self.read_for_write()?;
        let present = raw
            .get(spec.section)
            .and_then(Value::as_object)
            .is_some_and(|s| s.contains_key(spec.json_key));
        if !present {
            return Ok(false);
        }
        stamp_schema_version(&mut raw);
        let section = raw
            .get_mut(spec.section)
            .and_then(Value::as_object_mut)
            .expect("checked above");
        section.shift_remove(spec.json_key);
        if section.is_empty() {
            raw.shift_remove(spec.section);
        }
        atomic_write_json(&self.path, &Value::Object(raw))?;
        Ok(true)
    }

    /// What upstream logs while loading the `ui` section at startup, before
    /// logging is configured, so it reaches stderr on every command.
    pub fn ui_load_warnings(&self) -> Vec<String> {
        self.choice_load_warnings("ui")
    }

    /// One lenient read of `section`: the unreadable-file warning, else one
    /// per stored choice value that is not a valid choice.
    fn choice_load_warnings(&self, section: &str) -> Vec<String> {
        let (raw, warning) = self.read_lenient_with_warning();
        if let Some(warning) = warning {
            return vec![warning];
        }
        let Some(Value::Object(stored)) = raw.get(section) else {
            return Vec::new();
        };
        self.registry
            .specs()
            .filter(|spec| spec.section == section)
            .filter_map(|spec| match (stored.get(spec.json_key), spec.kind) {
                (Some(value), Kind::Choice(choices))
                    if !value.as_str().is_some_and(|v| choices.contains(&v)) =>
                {
                    Some(format!(
                        "settings.json: unsupported {} {}; using {}",
                        spec.dotted(),
                        python_repr(value),
                        python_repr(&spec.default_value().to_json())
                    ))
                }
                _ => None,
            })
            .collect()
    }

    fn read_lenient_with_warning(&self) -> (Map<String, Value>, Option<String>) {
        let path = self.path.display();
        let unreadable = |reason: String| {
            (
                Map::new(),
                Some(format!("Could not read {path} ({reason}); using defaults")),
            )
        };
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Map::new(), None),
            Err(e) => return unreadable(python_os_error(&e, &self.path)),
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(e) => return unreadable(python_utf8_error(&bytes, &e)),
        };
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => (map, None),
            Ok(_) => (
                Map::new(),
                Some(format!("{path} is not a JSON object; using defaults")),
            ),
            Err(e) => unreadable(e.to_string()),
        }
    }

    /// A corrupt file is an error here: rewriting from `{}` would replace a
    /// hand-recoverable file with a near-empty one.
    fn read_for_write(&self) -> Result<Map<String, Value>> {
        let path = self.path.display();
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => {
                return Err(Error::config(format!(
                    "could not read {path}: {}",
                    python_os_error(&e, &self.path)
                )));
            }
        };
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            Error::config(format!(
                "could not read {path}: {}",
                python_utf8_error(&bytes, &e)
            ))
        })?;
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) => Err(Error::config(format!(
                "{path} is not a JSON object; fix or delete it before changing settings"
            ))),
            Err(e) => Err(Error::config(format!(
                "{path} is not valid JSON ({e}); fix or delete it before changing settings"
            ))),
        }
    }
}

fn stamp_schema_version(raw: &mut Map<String, Value>) {
    if !raw.contains_key("schemaVersion") {
        raw.insert("schemaVersion".into(), SETTINGS_SCHEMA_VERSION.into());
    }
}

/// Writes `data` as `json.dumps(indent=2)` through a symlink (never over
/// it), with the temp file beside the resolved target so the rename stays on
/// one filesystem. The 0700 goes on the directory holding `path`, not on the
/// link target's, which may belong to someone else.
pub fn atomic_write_json(path: &Path, data: &Value) -> Result<()> {
    let io = |e: std::io::Error| Error::config(format!("could not write {}: {e}", path.display()));
    let target = resolve_link(path);
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    if let Some(own_dir) = path.parent() {
        std::fs::set_permissions(own_dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    let (tmp, mut file) = create_temp(dir).map_err(io)?;
    let written = file
        .write_all(pyjson::to_string_pretty(data).as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, &target))
        .and_then(|()| std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

/// `os.path.realpath` for a symlink, including a dangling one.
fn resolve_link(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::read_link(&current) {
            Ok(next) => {
                current = match current.parent() {
                    Some(parent) if next.is_relative() => parent.join(next),
                    _ => next,
                };
            }
            Err(_) => break,
        }
    }
    if current == path {
        return current;
    }
    match (current.parent(), current.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or(current),
        _ => current,
    }
}

fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("tmp{:x}{nanos:x}{n:x}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `repr()` of the value `json.loads` would have produced.
fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => pyjson::float_repr(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => python_str_repr(s),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(python_repr).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", python_str_repr(k), python_repr(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn python_str_repr(s: &str) -> String {
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
            c if c.is_control() => match c as u32 {
                n @ 0..=0xff => out.push_str(&format!("\\x{n:02x}")),
                n => out.push_str(&format!("\\u{n:04x}")),
            },
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn python_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// Python allows `_` only between two digits.
fn without_digit_underscores(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let before = i.checked_sub(1).map(|j| chars[j]);
            let after = chars.get(i + 1).copied();
            if !(before.is_some_and(|c| c.is_ascii_digit())
                && after.is_some_and(|c| c.is_ascii_digit()))
            {
                return None;
            }
        }
    }
    Some(s.replace('_', ""))
}

/// `int(str)`: surrounding whitespace, a sign, digits with `_` separators.
fn python_int(raw: &str) -> Option<i128> {
    let s = without_digit_underscores(python_strip(raw))?;
    let digits = s.strip_prefix(['+', '-']).unwrap_or(&s);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let negative = s.starts_with('-');
    match s.parse::<i128>() {
        Ok(v) => Some(v),
        Err(_) if negative => Some(i128::MIN),
        Err(_) => Some(i128::MAX),
    }
}

/// `float(str)`: Rust's grammar plus whitespace and `_` separators.
pub fn python_float(raw: &str) -> Option<f64> {
    let s = without_digit_underscores(python_strip(raw))?;
    s.parse::<f64>().ok()
}

fn python_os_error(e: &std::io::Error, path: &Path) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let text = e.to_string();
            let desc = text
                .strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text);
            format!("[Errno {code}] {desc}: '{}'", path.display())
        }
        None => e.to_string(),
    }
}

fn python_utf8_error(bytes: &[u8], e: &std::str::Utf8Error) -> String {
    let pos = e.valid_up_to();
    let byte = bytes[pos];
    let reason = match e.error_len() {
        None => "unexpected end of data",
        Some(_) if matches!(byte, 0x80..=0xc1 | 0xf5..=0xff) => "invalid start byte",
        Some(_) => "invalid continuation byte",
    };
    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {pos}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_number_grammar() {
        assert_eq!(python_int(" 5 "), Some(5));
        assert_eq!(python_int("1_0"), Some(10));
        assert_eq!(python_int("+7"), Some(7));
        for bad in ["3.5", "", "1e2", "_1", "1__0", "1_", "x"] {
            assert_eq!(python_int(bad), None, "{bad}");
        }
        assert_eq!(python_float(" 80 "), Some(80.0));
        assert_eq!(python_float("1_0.5"), Some(10.5));
        assert_eq!(python_float("1e2"), Some(100.0));
        assert!(python_float("nan").unwrap().is_nan());
        assert_eq!(python_float("high"), None);
    }
}
