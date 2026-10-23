//! The dropdown of upstream `menubar.py::rebuild_menu`.

use super::*;
use crate::menubar::format::{DisplayUsage, MenuAccount};
use serde_json::json;

const NOW: f64 = 1_000_000.0;

fn outline(entries: &[Entry], depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    for entry in entries {
        match entry {
            Entry::Separator => out.push(format!("{pad}---")),
            Entry::Item { title, checked, .. } => {
                out.push(format!("{pad}{}{title}", if *checked { "✓ " } else { "" }))
            }
            Entry::Submenu { title, entries } => {
                out.push(format!("{pad}{title} ▸"));
                outline(entries, depth + 1, out);
            }
        }
    }
}

fn render(input: &MenuInput) -> Vec<String> {
    let mut out = Vec::new();
    outline(&build_menu(input), 0, &mut out);
    out
}

fn two_accounts() -> MenuSnapshot {
    let usage = json!({"five_hour": {"pct": 42.0}, "seven_day": {"pct": 18.0}});
    let account =
        |number: &str, email: &str, alias: &str, active: bool, disabled: bool| MenuAccount {
            number: number.into(),
            email: email.into(),
            is_active: active,
            display: DisplayUsage::Measured(usage.clone()),
            last_good: Some(usage.clone()),
            alias: alias.into(),
            disabled,
            fetched_at: None,
        };
    MenuSnapshot {
        accounts: vec![
            account("1", "a@x.com", "work", true, false),
            account("2", "b@x.com", "", false, true),
        ],
        active_email: Some("a@x.com".into()),
        active_usage: DisplayUsage::Measured(usage.clone()),
        active_alias: Some("work".into()),
        codex: Vec::new(),
    }
}

fn with_codex() -> MenuSnapshot {
    let at = |s: f64| {
        chrono::DateTime::from_timestamp((NOW + s) as i64, 0)
            .unwrap()
            .to_rfc3339()
    };
    let account = |number: &str, email: &str, alias: &str, active: bool, usage| MenuAccount {
        number: number.into(),
        email: email.into(),
        is_active: active,
        display: DisplayUsage::Measured(usage),
        last_good: None,
        alias: alias.into(),
        disabled: false,
        fetched_at: None,
    };
    MenuSnapshot {
        codex: vec![
            account(
                "1",
                "c@x.com",
                "",
                false,
                json!({
                    "five_hour": {"pct": 10.0, "resets_at": at(3600.0 + 5.0 * 60.0)},
                    "seven_day": {"pct": 20.0, "resets_at": at(3.0 * 86400.0 + 2.0 * 3600.0)},
                }),
            ),
            account(
                "2",
                "d@x.com",
                "plus",
                true,
                json!({"five_hour": {"pct": 5.0}, "seven_day": {"pct": 60.0}}),
            ),
        ],
        ..two_accounts()
    }
}

fn codex_card(entries: &[Entry], number: &str) -> AccountCard {
    entries
        .iter()
        .find_map(|e| match e {
            Entry::Item {
                command: Some(Command::CodexSwitchTo(n)),
                card: Some(card),
                ..
            } if n == number => Some(card.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Codex card for account {number}"))
}

#[test]
fn codex_accounts_follow_the_claude_accounts_under_a_heading() {
    let lines = render(&MenuInput {
        snapshot: &with_codex(),
        settings: &MenuBarSettings::default(),
        history: &[],
        threshold: 95,
        now: NOW,
    });
    let start = lines.iter().position(|l| l == "Codex").unwrap();
    assert_eq!(
        lines[start - 2..start + 5],
        [
            "2  b@x.com  (disabled)  5h 42% · 7d 18%",
            "---",
            "Codex",
            "1  c@x.com  5h 10% (1h 5m) · 7d 20% (3d 2h)",
            "---",
            "✓ 2  plus  (d@x.com)  5h 5% · 7d 60%",
            "---",
        ]
    );
    assert_eq!(lines[start + 5], "Rotate to next");
    let tools = lines.iter().position(|l| l == "Codex accounts ▸").unwrap();
    assert_eq!(lines[tools - 1], "Refresh current credentials");
    assert_eq!(
        lines[tools..tools + 6],
        [
            "Codex accounts ▸",
            "  Add from current login",
            "  Log in to a new account…",
            "  Remove account ▸",
            "    1  c@x.com",
            "    2  plus  (d@x.com)",
        ]
    );
}

#[test]
fn codex_cards_show_the_active_mark_usage_and_reset() {
    let menu = menu_for(&with_codex());
    let first = codex_card(&menu, "1");
    assert_eq!(first.status, AccountStatus::Idle);
    assert_eq!(
        rows_of(&first),
        [
            ("Session".into(), "10%".into(), Some("1h 5m".into())),
            ("All".into(), "20%".into(), Some("3d 2h".into())),
        ]
    );
    assert_eq!(codex_card(&menu, "2").status, AccountStatus::Active);
}

#[test]
fn codex_entries_carry_codex_commands_apart_from_claude_ones() {
    let menu = menu_for(&with_codex());
    assert_eq!(
        command(&menu, "1  c@x.com  5h 10% (1h 5m) · 7d 20% (3d 2h)"),
        Command::CodexSwitchTo("1".into())
    );
    assert_eq!(
        command(&menu, "1  work  (a@x.com)  5h 42% · 7d 18%"),
        Command::SwitchTo("1".into())
    );
    let Entry::Submenu { entries, .. } = find(&menu, "Codex accounts") else {
        panic!()
    };
    assert_eq!(
        command(entries, "Add from current login"),
        Command::CodexAddFromLogin
    );
    assert_eq!(
        command(entries, "Log in to a new account…"),
        Command::CodexLogin
    );
    let Entry::Submenu { entries, .. } = find(entries, "Remove account") else {
        panic!()
    };
    assert_eq!(
        command(entries, "2  plus  (d@x.com)"),
        Command::CodexRemove("2".into())
    );
}

fn find<'a>(entries: &'a [Entry], title: &str) -> &'a Entry {
    entries
        .iter()
        .find(|e| match e {
            Entry::Item { title: t, .. } | Entry::Submenu { title: t, .. } => t == title,
            Entry::Separator => false,
        })
        .unwrap_or_else(|| panic!("no entry {title:?}"))
}

fn command(entries: &[Entry], title: &str) -> Command {
    match find(entries, title) {
        Entry::Item {
            command: Some(c), ..
        } => c.clone(),
        other => panic!("{title:?} has no command: {other:?}"),
    }
}

#[test]
fn full_menu_matches_upstream_layout() {
    let snapshot = two_accounts();
    let settings = MenuBarSettings::default();
    let history = ["2 → 1   2026-06-27 02:10".to_string()];
    let lines = render(&MenuInput {
        snapshot: &snapshot,
        settings: &settings,
        history: &history,
        threshold: 95,
        now: NOW,
    });
    assert_eq!(
        lines,
        [
            "✓ 1  work  (a@x.com)  5h 42% · 7d 18%",
            "---",
            "2  b@x.com  (disabled)  5h 42% · 7d 18%",
            "---",
            "Rotate to next",
            "Switch to best",
            "Next available",
            "---",
            "Add account ▸",
            "  From current login",
            "  From setup-token…",
            "Disable / enable account ▸",
            "  1  work  (a@x.com)",
            "  ✓ 2  b@x.com",
            "Remove account ▸",
            "  1  work  (a@x.com)",
            "  2  b@x.com",
            "Refresh current credentials",
            "Switch history ▸",
            "  2 → 1   2026-06-27 02:10",
            "  ---",
            "  Open full log…",
            "---",
            "Settings ▸",
            "  ✓ Show account name in menu bar",
            "  Title percentage ▸",
            "    None",
            "    Session (5h)",
            "    Weekly (7d)",
            "    ✓ Both (5h · 7d)",
            "  Show model limits in title",
            "  Refresh interval ▸",
            "    30 seconds",
            "    ✓ 60 seconds",
            "    5 minutes",
            "  Auto-switch accounts",
            "  Auto-switch threshold ▸",
            "    80%",
            "    90%",
            "    ✓ 95%",
            "    98%",
            "Refresh now",
            "Quit",
        ]
    );
}

#[test]
fn empty_store_shows_placeholders() {
    let snapshot = MenuSnapshot::default();
    let settings = MenuBarSettings {
        title_scoped: true,
        refresh_interval: 300,
        auto_switch_enabled: true,
        title_pct: "off".into(),
        show_account_name: false,
    };
    let lines = render(&MenuInput {
        snapshot: &snapshot,
        settings: &settings,
        history: &[],
        threshold: 0,
        now: NOW,
    });
    assert_eq!(lines[0], "No managed accounts");
    let after = |title: &str| lines[lines.iter().position(|l| l == title).unwrap() + 1].clone();
    assert_eq!(after("Disable / enable account ▸"), "  No managed accounts");
    assert_eq!(after("Remove account ▸"), "  No managed accounts");
    assert_eq!(after("Switch history ▸"), "  No switches logged yet");
    for checked in [
        "    ✓ None",
        "  ✓ Show model limits in title",
        "    ✓ 5 minutes",
        "  ✓ Auto-switch accounts",
    ] {
        assert!(lines.iter().any(|l| l == checked), "{checked} missing");
    }
    assert!(lines.iter().any(|l| l == "  Show account name in menu bar"));
    assert!(!lines.iter().any(|l| l.contains("✓ 8") || l.contains("✓ 9")));
}

#[test]
fn entries_carry_their_commands() {
    let snapshot = two_accounts();
    let settings = MenuBarSettings::default();
    let menu = build_menu(&MenuInput {
        snapshot: &snapshot,
        settings: &settings,
        history: &[],
        threshold: 90,
        now: NOW,
    });
    assert_eq!(
        command(&menu, "1  work  (a@x.com)  5h 42% · 7d 18%"),
        Command::SwitchTo("1".into())
    );
    assert_eq!(command(&menu, "Rotate to next"), Command::Switch(None));
    assert_eq!(
        command(&menu, "Switch to best"),
        Command::Switch(Some(Strategy::Best))
    );
    assert_eq!(
        command(&menu, "Next available"),
        Command::Switch(Some(Strategy::NextAvailable))
    );
    assert_eq!(command(&menu, "Quit"), Command::Quit);
    let Entry::Submenu { entries, .. } = find(&menu, "Disable / enable account") else {
        panic!()
    };
    assert_eq!(
        command(entries, "2  b@x.com"),
        Command::SetDisabled {
            number: "2".into(),
            disabled: false
        }
    );
    assert_eq!(
        command(entries, "1  work  (a@x.com)"),
        Command::SetDisabled {
            number: "1".into(),
            disabled: true
        }
    );
    let Entry::Submenu { entries, .. } = find(&menu, "Remove account") else {
        panic!()
    };
    assert_eq!(command(entries, "2  b@x.com"), Command::Remove("2".into()));
}

fn card(entries: &[Entry], number: &str) -> AccountCard {
    entries
        .iter()
        .find_map(|e| match e {
            Entry::Item {
                command: Some(Command::SwitchTo(n)),
                card: Some(card),
                ..
            } if n == number => Some(card.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no card for account {number}"))
}

fn rows_of(card: &AccountCard) -> Vec<(String, String, Option<String>)> {
    card.usage
        .as_ref()
        .unwrap()
        .iter()
        .map(|r| (r.name().to_string(), r.pct_text(), r.countdown.clone()))
        .collect()
}

fn snapshot_of(accounts: Vec<MenuAccount>) -> MenuSnapshot {
    MenuSnapshot {
        accounts,
        ..MenuSnapshot::default()
    }
}

fn measured_account(number: &str, usage: serde_json::Value) -> MenuAccount {
    MenuAccount {
        number: number.into(),
        email: format!("{number}@x.com"),
        is_active: false,
        display: DisplayUsage::Measured(usage.clone()),
        last_good: Some(usage),
        alias: String::new(),
        disabled: false,
        fetched_at: None,
    }
}

fn menu_for(snapshot: &MenuSnapshot) -> Vec<Entry> {
    build_menu(&MenuInput {
        snapshot,
        settings: &MenuBarSettings::default(),
        history: &[],
        threshold: 95,
        now: NOW,
    })
}

#[test]
fn account_cards_show_one_bar_per_window_with_live_countdowns() {
    let at = |s: f64| {
        chrono::DateTime::from_timestamp((NOW + s) as i64, 0)
            .unwrap()
            .to_rfc3339()
    };
    let usage = json!({
        "five_hour": {"pct": 64.0, "resets_at": at(2.0 * 3600.0 + 49.0 * 60.0)},
        "seven_day": {"pct": 77.0, "resets_at": at(16.0 * 3600.0 + 9.0 * 60.0)},
        "scoped": [{"name": "Fable", "pct": 0.0, "resets_at": at(16.0 * 3600.0 + 9.0 * 60.0)}],
    });
    let snapshot = snapshot_of(vec![measured_account("1", usage)]);
    let card = card(&menu_for(&snapshot), "1");
    assert_eq!(
        rows_of(&card),
        [
            ("Session".into(), "64%".into(), Some("2h 49m".into())),
            ("All".into(), "77%".into(), Some("16h 9m".into())),
            ("Fable".into(), "0%".into(), Some("16h 9m".into())),
        ]
    );
}

#[test]
fn account_card_names_the_alias_and_keeps_the_email_beside_it() {
    let snapshot = two_accounts();
    let menu = menu_for(&snapshot);
    let work = card(&menu, "1");
    assert_eq!(
        (work.name.as_str(), work.email.as_deref()),
        ("work", Some("a@x.com"))
    );
    let plain = card(&menu, "2");
    assert_eq!((plain.name.as_str(), plain.email), ("b@x.com", None));
}

#[test]
fn account_card_status_prefers_active_then_disabled_then_limit() {
    let mut active = measured_account("1", json!({"five_hour": {"pct": 100.0}}));
    active.is_active = true;
    let mut disabled = measured_account("2", json!({"five_hour": {"pct": 100.0}}));
    disabled.disabled = true;
    let limited = measured_account("3", json!({"five_hour": {"pct": 100.0}}));
    let weekly_maxed = measured_account(
        "4",
        json!({"five_hour": {"pct": 10.0}, "seven_day": {"pct": 100.0}}),
    );
    let snapshot = snapshot_of(vec![active, disabled, limited, weekly_maxed]);
    let menu = menu_for(&snapshot);
    let statuses: Vec<_> = ["1", "2", "3", "4"]
        .iter()
        .map(|n| card(&menu, n).status)
        .collect();
    assert_eq!(
        statuses,
        [
            AccountStatus::Active,
            AccountStatus::Disabled,
            AccountStatus::LimitReached,
            AccountStatus::Idle,
        ]
    );
}

#[test]
fn account_card_without_usage_carries_the_note() {
    let mut account = measured_account("1", json!({}));
    account.display = DisplayUsage::Note("no credentials".into());
    let snapshot = snapshot_of(vec![account]);
    assert_eq!(
        card(&menu_for(&snapshot), "1").usage,
        Err("no credentials".into())
    );
}
