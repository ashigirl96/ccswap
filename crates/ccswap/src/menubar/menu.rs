//! The dropdown as data (upstream `rebuild_menu`): AppKit only renders it,
//! so the structure is testable without a window server.

use ccswap_core::ranking::Strategy;

use super::format::{MenuSnapshot, format_account_label};
use super::settings::{MenuBarSettings, REFRESH_CHOICES, TITLE_PCT_CHOICES};

pub const AUTO_THRESHOLD_CHOICES: [i64; 4] = [80, 90, 95, 98];

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    SwitchTo(String),
    /// `None` = plain rotation.
    Switch(Option<Strategy>),
    AddFromLogin,
    AddFromToken,
    SetDisabled {
        number: String,
        disabled: bool,
    },
    Remove(String),
    RefreshCredentials,
    OpenLog,
    ToggleAccountName,
    TitlePct(&'static str),
    ToggleScoped,
    RefreshInterval(i64),
    ToggleAutoSwitch,
    Threshold(i64),
    RefreshNow,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Item {
        title: String,
        checked: bool,
        command: Option<Command>,
    },
    Submenu {
        title: String,
        entries: Vec<Entry>,
    },
    Separator,
}

impl Entry {
    fn action(title: impl Into<String>, command: Command) -> Self {
        Entry::Item {
            title: title.into(),
            checked: false,
            command: Some(command),
        }
    }

    fn label(title: impl Into<String>) -> Self {
        Entry::Item {
            title: title.into(),
            checked: false,
            command: None,
        }
    }

    fn check(self, on: bool) -> Self {
        match self {
            Entry::Item { title, command, .. } => Entry::Item {
                title,
                checked: on,
                command,
            },
            other => other,
        }
    }

    fn submenu(title: &str, entries: Vec<Entry>) -> Self {
        Entry::Submenu {
            title: title.into(),
            entries,
        }
    }
}

pub struct MenuInput<'a> {
    pub snapshot: &'a MenuSnapshot,
    pub settings: &'a MenuBarSettings,
    pub history: &'a [String],
    /// `autoswitch.threshold` as a whole percent, 0 when unreadable.
    pub threshold: i64,
    pub now: f64,
}

const NO_ACCOUNTS: &str = "No managed accounts";

pub fn build_menu(input: &MenuInput) -> Vec<Entry> {
    let accounts = &input.snapshot.accounts;
    let mut menu: Vec<Entry> = accounts
        .iter()
        .map(|a| {
            Entry::action(
                format_account_label(a, input.now),
                Command::SwitchTo(a.number.clone()),
            )
            .check(a.is_active)
        })
        .collect();
    if menu.is_empty() {
        menu.push(Entry::label(NO_ACCOUNTS));
    }
    menu.extend([
        Entry::Separator,
        Entry::action("Rotate to next", Command::Switch(None)),
        Entry::action("Switch to best", Command::Switch(Some(Strategy::Best))),
        Entry::action(
            "Next available",
            Command::Switch(Some(Strategy::NextAvailable)),
        ),
        Entry::Separator,
        Entry::submenu(
            "Add account",
            vec![
                Entry::action("From current login", Command::AddFromLogin),
                Entry::action("From setup-token…", Command::AddFromToken),
            ],
        ),
        disable_menu(input.snapshot),
        remove_menu(input.snapshot),
        Entry::action("Refresh current credentials", Command::RefreshCredentials),
        history_menu(input.history),
        Entry::Separator,
        settings_menu(input.settings, input.threshold),
        Entry::action("Refresh now", Command::RefreshNow),
        Entry::action("Quit", Command::Quit),
    ]);
    menu
}

fn disable_menu(snapshot: &MenuSnapshot) -> Entry {
    let mut entries: Vec<Entry> = snapshot
        .accounts
        .iter()
        .map(|a| {
            Entry::action(
                format!("{}  {}", a.number, a.name()),
                Command::SetDisabled {
                    number: a.number.clone(),
                    disabled: !a.disabled,
                },
            )
            .check(a.disabled)
        })
        .collect();
    if entries.is_empty() {
        entries.push(Entry::label(NO_ACCOUNTS));
    }
    Entry::submenu("Disable / enable account", entries)
}

fn remove_menu(snapshot: &MenuSnapshot) -> Entry {
    let mut entries: Vec<Entry> = snapshot
        .accounts
        .iter()
        .map(|a| {
            Entry::action(
                format!("{}  {}", a.number, a.name()),
                Command::Remove(a.number.clone()),
            )
        })
        .collect();
    if entries.is_empty() {
        entries.push(Entry::label(NO_ACCOUNTS));
    }
    Entry::submenu("Remove account", entries)
}

fn history_menu(history: &[String]) -> Entry {
    let mut entries: Vec<Entry> = history.iter().map(Entry::label).collect();
    if entries.is_empty() {
        entries.push(Entry::label("No switches logged yet"));
    }
    entries.push(Entry::Separator);
    entries.push(Entry::action("Open full log…", Command::OpenLog));
    Entry::submenu("Switch history", entries)
}

fn settings_menu(settings: &MenuBarSettings, threshold: i64) -> Entry {
    let title_pct = TITLE_PCT_CHOICES
        .iter()
        .map(|&mode| {
            let label = match mode {
                "off" => "None",
                "5h" => "Session (5h)",
                "7d" => "Weekly (7d)",
                _ => "Both (5h · 7d)",
            };
            Entry::action(label, Command::TitlePct(mode)).check(settings.title_pct == mode)
        })
        .collect();
    let interval = REFRESH_CHOICES
        .iter()
        .map(|&secs| {
            let label = match secs {
                30 => "30 seconds",
                60 => "60 seconds",
                _ => "5 minutes",
            };
            Entry::action(label, Command::RefreshInterval(secs))
                .check(settings.refresh_interval == secs)
        })
        .collect();
    let thresholds = AUTO_THRESHOLD_CHOICES
        .iter()
        .map(|&pct| {
            Entry::action(format!("{pct}%"), Command::Threshold(pct)).check(threshold == pct)
        })
        .collect();
    Entry::submenu(
        "Settings",
        vec![
            Entry::action("Show account name in menu bar", Command::ToggleAccountName)
                .check(settings.show_account_name),
            Entry::submenu("Title percentage", title_pct),
            Entry::action("Show model limits in title", Command::ToggleScoped)
                .check(settings.title_scoped),
            Entry::submenu("Refresh interval", interval),
            Entry::action("Auto-switch accounts", Command::ToggleAutoSwitch)
                .check(settings.auto_switch_enabled),
            Entry::submenu("Auto-switch threshold", thresholds),
        ],
    )
}

#[cfg(test)]
mod tests;
