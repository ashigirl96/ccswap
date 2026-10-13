//! The engine's policy knobs: the `autoswitch` section of settings.json,
//! read leniently, with `auto`'s flags laid over it.

use serde_json::{Value, json};

use crate::paths::Paths;
use crate::settings::{SettingValue, Settings, setting_spec};

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
    setting_spec(dotted)
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
        let defaults = AutoSwitchSettings::default();
        let mut out = defaults.clone();
        for row in Settings::new(paths).effective() {
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
            .map(crate::settings::parse_model_names)
            .unwrap_or_default()
    }
}
