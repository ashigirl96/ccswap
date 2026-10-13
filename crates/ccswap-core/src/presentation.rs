use std::io::IsTerminal;

use serde_json::{Map, Value, json};

use crate::appearance::{self, Background};
use crate::env::Environment;
use crate::error::Error;
use crate::paths::Paths;
use crate::status::StatusReport;
use crate::usage_store::UsageEntry;

pub const SCHEMA_VERSION: i64 = 1;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

struct Palette {
    accent: &'static str,
    muted: &'static str,
    red: &'static str,
    yellow: &'static str,
}

const DARK: Palette = Palette {
    accent: "\x1b[38;5;173m",
    muted: "\x1b[38;5;250m",
    red: "\x1b[31m",
    yellow: "\x1b[33m",
};

const LIGHT: Palette = Palette {
    accent: "\x1b[38;2;149;76;42m",
    muted: "\x1b[38;2;99;93;85m",
    red: "\x1b[38;2;173;49;40m",
    yellow: "\x1b[38;2;121;89;17m",
};

/// Terminal styling: ANSI codes when colour is on, the text verbatim otherwise.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    enabled: bool,
    theme: Theme,
}

impl Style {
    pub fn new(enabled: bool, theme: Theme) -> Self {
        Style { enabled, theme }
    }

    pub fn plain() -> Self {
        Self::new(false, Theme::Dark)
    }

    /// `NO_COLOR` (even empty) disables, then `FORCE_COLOR` (any value)
    /// enables, then stdout must be a TTY with `TERM` other than `dumb`.
    pub fn detect(env: &Environment, paths: &Paths) -> Self {
        let enabled = if env.is_set("NO_COLOR") {
            false
        } else if env.is_set("FORCE_COLOR") {
            true
        } else {
            std::io::stdout().is_terminal() && env.get("TERM").is_none_or(|t| t != "dumb")
        };
        Self::new(enabled, cli_theme(paths))
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn palette(&self) -> &'static Palette {
        match self.theme {
            Theme::Dark => &DARK,
            Theme::Light => &LIGHT,
        }
    }

    fn paint(&self, text: &str, codes: &[&str]) -> String {
        if !self.enabled {
            return text.to_string();
        }
        format!("{}{text}{RESET}", codes.concat())
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(text, &[self.palette().accent])
    }

    pub fn muted(&self, text: &str) -> String {
        self.paint(text, &[self.palette().muted])
    }

    pub fn dimmed(&self, text: &str) -> String {
        self.paint(text, &[DIM])
    }

    pub fn bolded(&self, text: &str) -> String {
        self.paint(text, &[BOLD])
    }

    pub fn bold_accent(&self, text: &str) -> String {
        self.paint(text, &[BOLD, self.palette().accent])
    }

    pub fn yellowed(&self, text: &str) -> String {
        self.paint(text, &[self.palette().yellow])
    }

    pub fn red(&self, text: &str) -> String {
        self.paint(text, &[self.palette().red])
    }
}

/// The `ui.theme` setting; anything but `dark`/`light` reads as `auto`.
fn ui_theme(paths: &Paths) -> &'static str {
    let theme = std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|s| s.get("ui")?.get("theme")?.as_str().map(str::to_string));
    match theme.as_deref() {
        Some("dark") => "dark",
        Some("light") => "light",
        _ => "auto",
    }
}

/// `auto` follows the background [`probe_cli_background`] found, if any.
fn cli_theme(paths: &Paths) -> Theme {
    match appearance::resolve_theme(ui_theme(paths), appearance::detected_background) {
        Background::Light => Theme::Light,
        Background::Dark => Theme::Dark,
    }
}

/// Senses the terminal background once, before dispatch, when `ui.theme`
/// is `auto` and coloured human output is coming.
pub fn probe_cli_background(env: &Environment, paths: &Paths, argv: &[String]) {
    let probe = appearance::cli_should_probe(argv, Style::detect(env, paths).enabled());
    appearance::cli_theme(
        ui_theme(paths),
        || appearance::detect_terminal_background(env),
        probe,
    );
}

pub fn error_envelope(error: &Error) -> Value {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "error": { "type": error.kind.type_name(), "message": error.message },
    })
}

pub fn status_lines(report: &StatusReport, style: &Style) -> Vec<String> {
    let label = style.bolded("Status:");
    match report {
        StatusReport::NoActiveAccount => {
            vec![format!(
                "{label} {}",
                style.dimmed("No active Claude account")
            )]
        }
        StatusReport::Unmanaged { email } => {
            vec![format!("{label} {email} {}", style.dimmed("(not managed)"))]
        }
        StatusReport::Managed {
            number,
            email,
            organization_name,
            total_managed,
            usage,
            ..
        } => {
            let tag = if organization_name.is_empty() {
                "personal"
            } else {
                organization_name
            };
            let mut lines = vec![
                format!(
                    "{label} {} ({email} {})",
                    style.accent(&format!("Account-{number}")),
                    style.muted(&format!("[{tag}]"))
                ),
                format!(
                    "  {}",
                    style.dimmed(&format!("Total managed accounts: {total_managed}"))
                ),
            ];
            lines.extend(
                usage_lines(usage, style)
                    .into_iter()
                    .map(|l| format!("  {l}")),
            );
            lines
        }
    }
}

fn usage_lines(entry: &UsageEntry, style: &Style) -> Vec<String> {
    crate::usage_view::usage_entry_lines(entry, style)
}

pub fn status_json(report: &StatusReport) -> Value {
    let mut payload = Map::new();
    payload.insert("schemaVersion".into(), SCHEMA_VERSION.into());
    match report {
        StatusReport::NoActiveAccount => {
            payload.insert("active".into(), Value::Null);
        }
        StatusReport::Unmanaged { email } => {
            payload.insert("active".into(), json!({ "email": email, "managed": false }));
        }
        StatusReport::Managed {
            number,
            email,
            organization_name,
            organization_uuid,
            alias,
            total_managed,
            usage,
        } => {
            let mut active = Map::new();
            active.insert("number".into(), (*number).into());
            active.insert("email".into(), email.as_str().into());
            active.insert("organizationName".into(), organization_name.as_str().into());
            active.insert("organizationUuid".into(), organization_uuid.as_str().into());
            active.insert(
                "isOrganization".into(),
                (!organization_uuid.is_empty()).into(),
            );
            active.insert("managed".into(), true.into());
            crate::usage_view::insert_usage_fields(&mut active, usage, alias, false);
            payload.insert("active".into(), Value::Object(active));
            payload.insert("totalManagedAccounts".into(), (*total_managed).into());
        }
    }
    Value::Object(payload)
}
