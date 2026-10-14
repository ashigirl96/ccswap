//! The dropdown is plain data that AppKit only renders, so it is testable
//! without a window server.

use ccswap_core::ranking::Strategy;

use super::format::{
    MenuAccount, MenuSnapshot, UsageKind, UsageRow, format_account_label, usage_rows,
};
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
        /// Still read by VoiceOver and type-select when `card` replaces it.
        title: String,
        checked: bool,
        command: Option<Command>,
        card: Option<AccountCard>,
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
            card: None,
        }
    }

    fn label(title: impl Into<String>) -> Self {
        Entry::Item {
            title: title.into(),
            checked: false,
            command: None,
            card: None,
        }
    }

    fn check(self, on: bool) -> Self {
        match self {
            Entry::Item {
                title,
                command,
                card,
                ..
            } => Entry::Item {
                title,
                checked: on,
                command,
                card,
            },
            other => other,
        }
    }

    fn card(self, card: AccountCard) -> Self {
        match self {
            Entry::Item {
                title,
                checked,
                command,
                ..
            } => Entry::Item {
                title,
                checked,
                command,
                card: Some(card),
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AccountStatus {
    Active,
    Disabled,
    LimitReached,
    Idle,
}

impl AccountStatus {
    pub fn text(self) -> &'static str {
        match self {
            AccountStatus::Active => "In use",
            AccountStatus::Disabled => "Disabled",
            AccountStatus::LimitReached => "Limit reached",
            AccountStatus::Idle => "",
        }
    }
}

/// An account's dropdown row drawn as the widget draws it: a header and one
/// bar per usage window.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountCard {
    pub number: String,
    pub name: String,
    /// Shown beside an alias so the email stays findable.
    pub email: Option<String>,
    pub status: AccountStatus,
    /// `Err` is the note shown instead of bars.
    pub usage: Result<Vec<UsageRow>, String>,
}

fn account_card(account: &MenuAccount, now: f64) -> AccountCard {
    let usage = usage_rows(&account.display, now, account.fetched_at);
    let session_maxed = usage.as_ref().is_ok_and(|rows| {
        rows.iter()
            .any(|r| r.kind == UsageKind::Session && r.maxed())
    });
    let status = if account.is_active {
        AccountStatus::Active
    } else if account.disabled {
        AccountStatus::Disabled
    } else if session_maxed {
        AccountStatus::LimitReached
    } else {
        AccountStatus::Idle
    };
    let (name, email) = if account.alias.is_empty() {
        (account.email.clone(), None)
    } else {
        (account.alias.clone(), Some(account.email.clone()))
    };
    AccountCard {
        number: account.number.clone(),
        name,
        email,
        status,
        usage,
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
    let mut menu = Vec::new();
    for (i, a) in accounts.iter().enumerate() {
        if i > 0 {
            menu.push(Entry::Separator);
        }
        menu.push(
            Entry::action(
                format_account_label(a, input.now),
                Command::SwitchTo(a.number.clone()),
            )
            .check(a.is_active)
            .card(account_card(a, input.now)),
        );
    }
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
