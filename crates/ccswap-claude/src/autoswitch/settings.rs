//! The `autoswitch` section of settings.json, read leniently, with `auto`'s
//! flags laid over it.

use ccswap_core::paths::Paths;
use ccswap_core::settings::{Kind, SettingDefault, SettingSpec, SettingValue, Settings};
use serde_json::{Value, json};

/// The `autoswitch` section of settings.json.
pub static AUTOSWITCH_SETTINGS: &[SettingSpec] = &[
    SettingSpec {
        section: "autoswitch",
        json_key: "threshold",
        kind: Kind::Float { lo: 50.0, hi: 99.9 },
        default: SettingDefault::Float(90.0),
        help: "Switch when the binding 5h/7d window reaches this pct",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "intervalSeconds",
        kind: Kind::Float {
            lo: 15.0,
            hi: 3600.0,
        },
        default: SettingDefault::Float(60.0),
        help: "Poll interval for the cswap auto loop, in seconds",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "cooldownSeconds",
        kind: Kind::Float {
            lo: 0.0,
            hi: 86400.0,
        },
        default: SettingDefault::Float(300.0),
        help: "Minimum seconds between proactive switches",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "hysteresisPct",
        kind: Kind::Float { lo: 0.0, hi: 50.0 },
        default: SettingDefault::Float(10.0),
        help: "A target must beat the active account by this many pct",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "strategy",
        kind: Kind::Choice(&["best", "consume-first"]),
        default: SettingDefault::Text("best"),
        help: "How auto-switch picks the target account",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "includeApiKeyAccounts",
        kind: Kind::Bool,
        default: SettingDefault::Bool(false),
        help: "Allow rotating onto managed API-key accounts (bill per token)",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "unhealthyTicks",
        kind: Kind::Int { lo: 1, hi: 100 },
        default: SettingDefault::Int(3),
        help: "Consecutive failed polls before an account is unhealthy",
    },
    SettingSpec {
        section: "autoswitch",
        json_key: "model",
        kind: Kind::Text,
        default: SettingDefault::None,
        help: "Also switch on these models' weekly limits (e.g. Fable, Fable,Opus, or all)",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoStrategy {
    /// Most headroom.
    Best,
    /// Soonest weekly reset first.
    ConsumeFirst,
}

impl AutoStrategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "best" => Some(AutoStrategy::Best),
            "consume-first" => Some(AutoStrategy::ConsumeFirst),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AutoStrategy::Best => "best",
            AutoStrategy::ConsumeFirst => "consume-first",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoSwitchSettings {
    pub threshold: f64,
    pub interval_seconds: f64,
    pub cooldown_seconds: f64,
    pub hysteresis_pct: f64,
    pub strategy: AutoStrategy,
    pub include_api_key_accounts: bool,
    pub unhealthy_ticks: i64,
    /// Comma-separated model names or `all`; `None` watches 5h/7d only.
    pub model: Option<String>,
}

impl Default for AutoSwitchSettings {
    fn default() -> Self {
        AutoSwitchSettings {
            threshold: 90.0,
            interval_seconds: 60.0,
            cooldown_seconds: 300.0,
            hysteresis_pct: 10.0,
            strategy: AutoStrategy::Best,
            include_api_key_accounts: false,
            unhealthy_ticks: 3,
            model: None,
        }
    }
}

/// `auto`'s flags; `None` keeps the file's value.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliOverrides {
    pub threshold: Option<f64>,
    pub interval: Option<f64>,
    pub cooldown: Option<f64>,
    pub include_api_key_accounts: Option<bool>,
    pub model: Option<String>,
    pub strategy: Option<AutoStrategy>,
}

fn coerce(dotted: &str, raw: &Value) -> SettingValue {
    AUTOSWITCH_SETTINGS
        .iter()
        .find(|spec| spec.dotted() == dotted)
        .expect("registered setting")
        .lenient(Some(raw))
}

fn float(value: SettingValue, default: f64) -> f64 {
    match value {
        SettingValue::Float(f) => f,
        SettingValue::Int(i) => i as f64,
        _ => default,
    }
}

impl AutoSwitchSettings {
    /// The effective `autoswitch` section; missing or corrupt values fall
    /// back to defaults, numbers are clamped into range.
    pub fn load(paths: &Paths) -> Self {
        Self::from_settings(&Settings::new(paths, crate::SETTINGS))
    }

    pub fn from_settings(settings: &Settings) -> Self {
        let defaults = AutoSwitchSettings::default();
        let mut out = defaults.clone();
        for row in settings.effective() {
            if row.spec.section != "autoswitch" {
                continue;
            }
            match (row.spec.json_key, row.value) {
                ("threshold", v) => out.threshold = float(v, defaults.threshold),
                ("intervalSeconds", v) => {
                    out.interval_seconds = float(v, defaults.interval_seconds)
                }
                ("cooldownSeconds", v) => {
                    out.cooldown_seconds = float(v, defaults.cooldown_seconds)
                }
                ("hysteresisPct", v) => out.hysteresis_pct = float(v, defaults.hysteresis_pct),
                ("strategy", SettingValue::Text(s)) => {
                    out.strategy = AutoStrategy::parse(&s).unwrap_or(defaults.strategy);
                }
                ("includeApiKeyAccounts", SettingValue::Bool(b)) => {
                    out.include_api_key_accounts = b
                }
                ("unhealthyTicks", SettingValue::Int(i)) => out.unhealthy_ticks = i,
                ("model", SettingValue::Text(s)) => out.model = Some(s),
                _ => {}
            }
        }
        out
    }

    /// Lays the non-`None` flags over the settings and clamps them like a
    /// file value.
    pub fn merged_with_cli(&self, cli: &CliOverrides) -> Self {
        let mut out = self.clone();
        let d = AutoSwitchSettings::default();
        if let Some(t) = cli.threshold {
            out.threshold = float(coerce("autoswitch.threshold", &json!(t)), d.threshold);
        }
        if let Some(i) = cli.interval {
            out.interval_seconds = float(
                coerce("autoswitch.intervalSeconds", &json!(i)),
                d.interval_seconds,
            );
        }
        if let Some(c) = cli.cooldown {
            out.cooldown_seconds = float(
                coerce("autoswitch.cooldownSeconds", &json!(c)),
                d.cooldown_seconds,
            );
        }
        if let Some(b) = cli.include_api_key_accounts {
            out.include_api_key_accounts = b;
        }
        if let Some(m) = &cli.model {
            out.model = Some(m.clone()).filter(|m| !m.is_empty());
        }
        if let Some(s) = cli.strategy {
            out.strategy = s;
        }
        out
    }

    pub fn models(&self) -> Vec<String> {
        self.model
            .as_deref()
            .map(parse_model_names)
            .unwrap_or_default()
    }
}

/// The `autoswitch.model` names in settings.json.
pub fn autoswitch_models(settings: &Settings) -> Vec<String> {
    match settings.get("autoswitch.model").map(|s| s.value) {
        Ok(SettingValue::Text(t)) => parse_model_names(&t),
        _ => Vec::new(),
    }
}

/// Comma-separated names, trimmed, deduplicated case-insensitively (first
/// spelling wins).
pub fn parse_model_names(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !out.iter().any(|o| o.to_lowercase() == name.to_lowercase()) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_are_trimmed_and_deduplicated() {
        assert_eq!(
            parse_model_names(" Fable, fable ,Opus,,"),
            ["Fable", "Opus"]
        );
        assert!(parse_model_names("").is_empty());
    }
}
