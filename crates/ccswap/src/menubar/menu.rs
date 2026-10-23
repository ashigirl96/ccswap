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
    CodexSwitchTo(String),
    /// Same as `ccswap codex switch` without an argument.
    CodexSwitchNext,
    CodexAddFromLogin,
    CodexLogin,
    CodexRemove(String),
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

const CLAUDE: &str = "Claude Code";
const CODEX: &str = "Codex";

pub fn build_menu(input: &MenuInput) -> Vec<Entry> {
    let snapshot = input.snapshot;
    let sections = [
        provider_section(
            CLAUDE,
            &snapshot.accounts,
            Command::SwitchTo,
            claude_actions(&snapshot.accounts),
            input.now,
        ),
        provider_section(
            CODEX,
            &snapshot.codex,
            Command::CodexSwitchTo,
            codex_actions(&snapshot.codex),
            input.now,
        ),
    ];
    let mut menu = Vec::new();
    for section in sections.into_iter().flatten() {
        if !menu.is_empty() {
            menu.push(Entry::Separator);
        }
        menu.extend(section);
    }
    if menu.is_empty() {
        menu.push(Entry::label(NO_ACCOUNTS));
    }
    menu.extend([
        Entry::Separator,
        add_menu(),
        history_menu(input.history),
        Entry::Separator,
        settings_menu(input.settings, input.threshold),
        Entry::action("Refresh now", Command::RefreshNow),
        Entry::action("Quit", Command::Quit),
    ]);
    menu
}

/// `None` for a provider without accounts, whose section is hidden.
fn provider_section(
    heading: &str,
    accounts: &[MenuAccount],
    switch_to: fn(String) -> Command,
    actions: Vec<Entry>,
    now: f64,
) -> Option<Vec<Entry>> {
    if accounts.is_empty() {
        return None;
    }
    let mut section = vec![Entry::label(heading)];
    for (i, a) in accounts.iter().enumerate() {
        if i > 0 {
            section.push(Entry::Separator);
        }
        section.push(
            Entry::action(format_account_label(a, now), switch_to(a.number.clone()))
                .check(a.is_active)
                .card(account_card(a, now)),
        );
    }
    section.extend([
        Entry::Separator,
        Entry::submenu(&format!("{heading} accounts"), actions),
    ]);
    Some(section)
}

fn claude_actions(accounts: &[MenuAccount]) -> Vec<Entry> {
    vec![
        Entry::action("Rotate to next", Command::Switch(None)),
        Entry::action("Switch to best", Command::Switch(Some(Strategy::Best))),
        Entry::action(
            "Next available",
            Command::Switch(Some(Strategy::NextAvailable)),
        ),
        Entry::Separator,
        Entry::action("Add from current login", Command::AddFromLogin),
        Entry::action("Add from setup-token…", Command::AddFromToken),
        disable_menu(accounts),
        account_picker("Remove account", accounts, Command::Remove),
        Entry::Separator,
        Entry::action("Refresh current credentials", Command::RefreshCredentials),
    ]
}

fn codex_actions(accounts: &[MenuAccount]) -> Vec<Entry> {
    vec![
        Entry::action("Rotate to next", Command::CodexSwitchNext),
        Entry::Separator,
        Entry::action("Add from current login", Command::CodexAddFromLogin),
        Entry::action("Log in to a new account…", Command::CodexLogin),
        account_picker("Remove account", accounts, Command::CodexRemove),
    ]
}

/// Holds every provider's add items, so a provider whose section is hidden
/// still has a way in for its first account.
fn add_menu() -> Entry {
    Entry::submenu(
        "Add account",
        vec![
            Entry::label(CLAUDE),
            Entry::action("From current login", Command::AddFromLogin),
            Entry::action("From setup-token…", Command::AddFromToken),
            Entry::Separator,
            Entry::label(CODEX),
            Entry::action("From current login", Command::CodexAddFromLogin),
            Entry::action("Log in to a new account…", Command::CodexLogin),
        ],
    )
}

fn disable_menu(accounts: &[MenuAccount]) -> Entry {
    let entries = accounts
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
    Entry::submenu("Disable / enable account", entries)
}

fn account_picker(title: &str, accounts: &[MenuAccount], command: fn(String) -> Command) -> Entry {
    let entries = accounts
        .iter()
        .map(|a| {
            Entry::action(
                format!("{}  {}", a.number, a.name()),
                command(a.number.clone()),
            )
        })
        .collect();
    Entry::submenu(title, entries)
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
