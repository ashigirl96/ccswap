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
    }
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
