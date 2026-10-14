//! Ported from upstream tests/test_tui.py::TestDashboard.

mod auto;
mod watch;

use ccswap_core::snapshot::AccountsSnapshot;
use ccswap_core::usage::now_secs;
use serde_json::json;

use super::app::{Screen, Severity, merge_late};
use super::testing::{Call, Harness, account, entry, make_account};
use super::theme::{DARK, ThemeName};
use super::widgets::accounts_panel;

fn panel(h: &Harness) -> String {
    let lines = accounts_panel(
        h.app.snapshot.as_ref(),
        92,
        h.app.threshold_pct,
        true,
        now_secs(),
        &DARK,
    );
    super::testing::plain_lines(&lines)
}

fn is_dashboard(h: &Harness) -> bool {
    matches!(h.app.screen(), Screen::Dashboard(_))
}

#[test]
fn panel_shows_active_full_and_others_mini() {
    let h = Harness::new(vec![
        make_account("1", true, entry(Some(47.0), Some(63.0))),
        make_account("2", false, entry(Some(92.0), Some(71.0))),
    ]);
    let text = panel(&h);
    assert!(text.contains("user1@example.com") && text.contains("● active"));
    assert!(text.contains("resets"));
    assert!(text.contains("user2@example.com") && text.contains("92%"));
    let mini = text.split("user2@example.com").nth(1).unwrap();
    assert!(!mini.contains('━'));

    let screen = h.render(100, 32);
    assert!(screen.contains("● active") && screen.contains("5h 92% · 7d 71%"));
    assert!(screen.contains("┃"), "threshold tick on the active card");
}

#[test]
fn panel_orders_slots_and_separates_the_card() {
    let h = Harness::new(vec![
        account("1", false),
        account("2", true),
        account("3", false),
    ]);
    let text = panel(&h);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with(" 1  user1@example.com"));
    assert_eq!(lines[1], "");
    assert!(lines[2].starts_with(" 2  user2@example.com"));
    assert!(lines[lines.len() - 2].is_empty());
    assert!(lines[lines.len() - 1].starts_with(" 3  user3@example.com"));
}

#[test]
fn disabled_marker_on_active_card_and_mini() {
    let mut one = account("1", true);
    one.disabled = true;
    let mut two = account("2", false);
    two.disabled = true;
    let h = Harness::new(vec![one, two]);
    let text = panel(&h);
    assert!(text.contains("● active"));
    assert_eq!(text.matches("(disabled)").count(), 2);
}

#[test]
fn active_card_skips_absent_window_and_shows_scoped() {
    let mut e = entry(Some(47.0), None);
    e.last_good.as_mut().unwrap()["scoped"] = json!([{"name": "Fable", "pct": 62.0}]);
    let h = Harness::new(vec![make_account("1", true, e)]);
    let text = panel(&h);
    assert!(text.contains("5h"));
    assert!(!text.contains("7d"));
    assert!(!text.contains("usage unknown"));
    assert!(text.contains("Fable") && text.contains("62%"));
}

#[test]
fn mini_line_skips_absent_window() {
    let h = Harness::new(vec![
        account("1", true),
        make_account("2", false, entry(Some(92.0), None)),
    ]);
    let text = panel(&h);
    let mini = text.split("user2@example.com").nth(1).unwrap();
    assert!(mini.contains("5h 92%"));
    assert!(!mini.contains("7d"));
}

#[test]
fn empty_state_hint_in_panel() {
    let h = Harness::new(vec![]);
    assert!(panel(&h).contains("No managed accounts yet"));
    assert!(h.render(100, 32).contains("No managed accounts yet"));
}

#[test]
fn panel_without_active_login_says_so() {
    let h = Harness::new(vec![account("1", false)]);
    let lines = accounts_panel(h.app.snapshot.as_ref(), 92, None, false, now_secs(), &DARK);
    assert_eq!(
        super::testing::plain_lines(&lines),
        "no active managed login"
    );
}

#[test]
fn menu_is_default_navigation_and_nests() {
    let mut h = Harness::new(vec![account("1", true)]);
    assert_eq!(
        h.menu_ids(),
        [
            "switch",
            "watch",
            "auto",
            "add-menu",
            "disable-menu",
            "remove-menu",
            "theme-menu",
            "quit"
        ]
    );
    let screen = h.render(100, 32);
    for label in [
        "Switch account…",
        "Watch accounts",
        "Auto-switch view",
        "Add account…",
        "Disable / enable account…",
        "Remove account…",
        "Theme…",
        "Quit",
    ] {
        assert!(screen.contains(label), "{label}");
    }
    h.press(&["down", "down", "down", "enter"]);
    assert_eq!(h.menu_ids(), ["add-login", "add-token", "back"]);
    assert!(h.render(100, 32).contains("menu › add account"));
    h.press(&["escape"]);
    assert_eq!(h.menu_ids()[0], "switch");
}

#[test]
fn remove_menu_shows_alias_before_email() {
    let mut one = account("1", true);
    one.alias = "dev".into();
    let mut two = account("2", false);
    two.email = "plain@example.com".into();
    let mut h = Harness::new(vec![one, two]);
    h.menu_select("remove-menu");
    let labels = h.menu_labels();
    assert!(labels.iter().any(|l| l.contains("dev (user1@example.com)")));
    assert!(labels.iter().any(|l| l.contains("plain@example.com")));
    assert!(!labels.iter().any(|l| l.contains("(plain@example.com)")));
    assert_eq!(labels[0], "1  dev (user1@example.com)  [personal]");
}

#[test]
fn remove_menu_label_renders_bracket_tag_literally() {
    let mut one = account("1", true);
    one.org_name = "red".into();
    let mut h = Harness::new(vec![one]);
    h.menu_select("remove-menu");
    assert!(h.menu_labels().iter().any(|l| l.contains("[red]")));
    assert!(h.render(100, 32).contains("[red]"));
}

#[test]
fn back_menu_entry_pops_submenu() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.menu_select("add-menu");
    h.menu_select("back");
    assert_eq!(h.menu_ids()[0], "switch");
}

#[test]
fn vim_keys_move_menu_cursor() {
    let mut h = Harness::new(vec![account("1", true)]);
    assert_eq!(h.app.dashboard().index, 0);
    h.press(&["j"]);
    assert_eq!(h.app.dashboard().index, 1);
    h.press(&["k"]);
    assert_eq!(h.app.dashboard().index, 0);
    h.press(&["k"]);
    assert_eq!(h.app.dashboard().index, 0, "the cursor does not wrap");
}

#[test]
fn s_opens_switch_screen_and_enter_switches() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.press(&["s"]);
    let Screen::Switch(screen) = h.app.screen().clone() else {
        panic!("switch screen expected");
    };
    assert_eq!(screen.numbers, ["1", "2"]);
    assert_eq!(screen.index, Some(0));
    let rendered = h.render(100, 32);
    assert!(rendered.contains("switch to which account?"));
    assert!(rendered.contains(" 2  user2@example.com"));
    assert!(!rendered.contains("┃"), "no threshold tick on switch cards");
    h.press(&["down", "enter"]);
    h.settle();
    assert!(h.calls().contains(&Call::SwitchTo("2".into())));
    assert!(is_dashboard(&h));
    assert_eq!(
        h.app.snapshot.as_ref().unwrap().active_number.as_deref(),
        Some("2")
    );
    assert!(
        h.app
            .notifications
            .iter()
            .any(|n| n.message == "Switched to user2@example.com"
                && n.title.as_deref() == Some("Switch"))
    );
}

#[test]
fn switch_screen_starts_on_the_active_account() {
    let mut h = Harness::new(vec![account("1", false), account("2", true)]);
    h.press(&["s"]);
    let Screen::Switch(screen) = h.app.screen() else {
        panic!("switch screen expected");
    };
    assert_eq!(screen.index, Some(1));
}

#[test]
fn switch_screen_escape_backs_out() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.press(&["enter"]);
    assert!(matches!(h.app.screen(), Screen::Switch(_)));
    h.press(&["escape"]);
    assert!(is_dashboard(&h));
    for key in ["q", "s"] {
        h.press(&["s"]);
        h.press(&[key]);
        assert!(is_dashboard(&h));
    }
    assert!(!h.calls().iter().any(|c| matches!(c, Call::SwitchTo(_))));
}

#[test]
fn best_pick_runs_the_best_strategy_and_reports_no_switch() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.press(&["s", "b"]);
    h.settle();
    assert_eq!(h.calls(), [Call::Switch(Some("best"))]);
    let n = h.app.notifications.last().unwrap();
    assert_eq!(n.message, "no-better-target");
    assert_eq!(n.title.as_deref(), Some("No switch"));
    assert_eq!(n.severity, Severity::Warning);
}

#[test]
fn remove_via_menu_confirms_then_removes() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.menu_select("remove-menu");
    h.menu_select("remove:2");
    let Screen::Confirm(c) = h.app.screen() else {
        panic!("confirm modal expected");
    };
    assert_eq!(
        c.message,
        "Remove account 2 (user2@example.com)?\n\nIts stored credentials and config backup are deleted."
    );
    let screen = h.render(100, 32);
    assert!(screen.contains("Remove account") && screen.contains("Cancel"));
    assert!(screen.contains("← → · enter  ·  y remove  ·  n / esc cancel"));
    h.press(&["y"]);
    h.settle();
    assert!(h.calls().contains(&Call::Remove("2".into(), true)));
    assert!(
        h.app
            .notifications
            .iter()
            .any(|n| n.message == "Removed account 2")
    );
    assert_eq!(h.app.snapshot.as_ref().unwrap().accounts.len(), 1);
}

#[test]
fn remove_via_menu_cancel_is_safe() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.menu_select("remove-menu");
    h.menu_select("remove:1");
    h.press(&["n"]);
    h.settle();
    assert!(!h.calls().iter().any(|c| matches!(c, Call::Remove(..))));
    h.press(&["enter", "escape"]);
    h.settle();
    assert!(h.calls().is_empty());
}

#[test]
fn disable_via_menu_toggles_without_confirm() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.menu_select("disable-menu");
    h.menu_select("disable:2");
    h.settle();
    assert!(h.calls().contains(&Call::SetDisabled("2".into(), true)));
    assert_eq!(h.menu_ids()[0], "switch");
}

#[test]
fn disable_menu_row_reflects_state_and_re_enables() {
    let mut two = account("2", false);
    two.disabled = true;
    let mut h = Harness::new(vec![account("1", true), two]);
    h.menu_select("disable-menu");
    let labels = h.menu_labels();
    assert_eq!(labels[0], "1  user1@example.com   → disable");
    assert_eq!(labels[1], "2  user2@example.com  (disabled)   → enable");
    h.menu_select("disable:2");
    h.settle();
    assert!(h.calls().contains(&Call::SetDisabled("2".into(), false)));
}

#[test]
fn modal_arrow_keys_choose_button() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.menu_select("remove-menu");
    h.menu_select("remove:2");
    h.press(&["right", "enter"]);
    h.settle();
    assert!(!h.calls().iter().any(|c| matches!(c, Call::Remove(..))));
    h.press(&["enter"]);
    h.press(&["right", "left", "enter"]);
    h.settle();
    assert!(h.calls().contains(&Call::Remove("2".into(), true)));
}

#[test]
fn full_refresh_binding() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.press(&["f"]);
    h.settle();
    assert_eq!(h.fake.fetch_sets.lock().unwrap().last(), Some(&None));
    assert!(
        h.app
            .notifications
            .iter()
            .any(|n| n.message == "Refreshing usage…")
    );
}

#[test]
fn add_current_login_confirms_and_shows_output() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.menu_select("add-menu");
    h.menu_select("add-login");
    let Screen::Confirm(c) = h.app.screen() else {
        panic!("confirm modal expected");
    };
    assert_eq!(c.title, "Add account");
    h.press(&["y"]);
    h.settle();
    assert_eq!(h.calls(), [Call::Add(None, false)]);
    let Screen::Output(o) = h.app.screen() else {
        panic!("output modal expected");
    };
    assert_eq!(o.title, "Add current login");
    assert!(
        h.render(100, 32)
            .contains("Added Account 9: fresh@example.com")
    );
    h.press(&["escape"]);
    assert!(is_dashboard(&h));
}

#[test]
fn add_token_via_menu_passes_assume_yes() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.menu_select("add-menu");
    h.menu_select("add-token");
    assert!(matches!(h.app.screen(), Screen::AddToken(_)));
    let screen = h.render(100, 40);
    assert!(screen.contains("Add account from token"));
    assert!(screen.contains("enter add  ·  tab next field  ·  esc cancel"));
    h.type_text("sk-ant-oat01-test");
    assert!(
        !h.render(100, 40).contains("sk-ant-oat01-test"),
        "the token is masked"
    );
    h.press(&["tab", "tab"]);
    h.type_text("x5");
    h.press(&["tab", "enter"]);
    h.settle();
    assert!(h.calls().contains(&Call::AddToken(
        "sk-ant-oat01-test".into(),
        None,
        Some(5),
        true
    )));
}

#[test]
fn add_token_validates_before_submitting() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.app.add_token();
    h.press(&["enter"]);
    let error = |h: &Harness| match h.app.screen() {
        Screen::AddToken(m) => m.error.clone(),
        _ => panic!("add-token modal expected"),
    };
    assert_eq!(error(&h), "Token is required.");
    h.type_text("tok");
    h.press(&["tab", "tab"]);
    h.type_text("-");
    h.press(&["enter"]);
    assert_eq!(error(&h), "Slot must be a number.");
    h.type_text("0");
    h.press(&["enter"]);
    assert_eq!(error(&h), "Slot must be >= 1.");
    assert!(h.render(100, 40).contains("Slot must be >= 1."));
    h.press(&["escape"]);
    assert!(is_dashboard(&h));
    assert!(h.calls().is_empty());
}

#[test]
fn add_token_occupied_slot_asks_first() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.menu_select("add-menu");
    h.menu_select("add-token");
    h.type_text("sk-ant-oat01-test");
    h.press(&["tab", "tab"]);
    h.type_text("2");
    h.press(&["enter"]);
    let Screen::Confirm(c) = h.app.screen() else {
        panic!("overwrite confirm expected");
    };
    assert_eq!(
        c.message,
        "Slot 2 is occupied by user2@example.com. Overwrite?"
    );
    assert_eq!(c.title, "Overwrite slot");
    h.press(&["n"]);
    h.settle();
    assert!(!h.calls().iter().any(|c| matches!(c, Call::AddToken(..))));
}

#[test]
fn actions_are_single_flight() {
    let mut h = Harness::new(vec![account("1", true), account("2", false)]);
    h.app.do_switch("2");
    h.app.do_switch("1");
    h.settle();
    assert_eq!(h.calls(), [Call::SwitchTo("2".into())]);
    assert!(
        h.app
            .notifications
            .iter()
            .any(|n| n.message == "Another action is still running")
    );
}

#[test]
fn failed_action_opens_the_output_modal() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.app.add_token();
    h.type_text("bad");
    h.press(&["enter"]);
    h.settle();
    let Screen::Output(o) = h.app.screen() else {
        panic!("output modal expected");
    };
    assert_eq!(o.title, "Add account from token — failed");
    assert!(o.output.ends_with("Error: Token cannot be empty"));
    assert!(!h.app.busy);
}

#[test]
fn toggling_an_unknown_slot_does_nothing() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.app.do_toggle_disabled("9");
    assert!(!h.app.busy);
}

#[test]
fn theme_menu_marks_current_and_persists() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.menu_select("theme-menu");
    assert_eq!(h.menu_labels(), ["● dark", "  light", "  auto", "← back"]);
    h.menu_select("theme:light");
    assert_eq!(h.app.theme_name, ThemeName::Light);
    assert_eq!(h.menu_ids()[0], "switch");
    let saved = std::fs::read_to_string(h.backup_dir().join("settings.json")).unwrap();
    assert!(saved.contains(r#""theme": "light""#), "{saved}");
    h.press(&["ctrl+t"]);
    assert_eq!(h.app.theme_name, ThemeName::Auto);
    assert_eq!(h.app.notifications.last().unwrap().message, "Theme: auto");
}

#[test]
fn quit_from_key_and_menu() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.press(&["q"]);
    assert!(h.app.should_quit);
    let mut h = Harness::new(vec![account("1", true)]);
    h.menu_select("quit");
    assert!(h.app.should_quit);
}

#[test]
fn footer_shows_the_screen_bindings() {
    let mut h = Harness::new(vec![account("1", true)]);
    let footer = |h: &Harness| h.render(100, 32).lines().last().unwrap().to_string();
    assert!(footer(&h).contains("s Switch accounts"));
    assert!(footer(&h).contains("w Watch") && footer(&h).contains("q Quit"));
    h.press(&["s"]);
    assert!(footer(&h).contains("enter Switch") && footer(&h).contains("b Best pick"));
}

fn snap(accounts: Vec<ccswap_core::snapshot::AccountSnapshot>, taken_at: f64) -> AccountsSnapshot {
    AccountsSnapshot {
        active_number: Some("1".into()),
        accounts,
        taken_at,
    }
}

#[test]
fn late_older_result_merges_usage_only_for_the_same_account() {
    let mut current_one = account("1", true);
    current_one.alias = "new-meta".into();
    let mut replaced = account("2", false);
    replaced.email = "someone-else@example.com".into();
    let current = snap(vec![current_one, replaced], 200.0);

    let late_one = make_account("1", true, entry(Some(80.0), None));
    let late_two = make_account("2", false, entry(Some(99.0), None));
    let late = snap(vec![late_one.clone(), late_two], 100.0);

    let merged = merge_late(current, &late);
    assert_eq!(merged.accounts[0].alias, "new-meta");
    assert_eq!(merged.accounts[0].usage, late_one.usage);
    assert_eq!(merged.accounts[1].email, "someone-else@example.com");
    assert_ne!(
        merged.accounts[1].usage.last_good.as_ref().unwrap()["five_hour"]["pct"],
        99.0
    );
    assert_eq!(merged.taken_at, 200.0);
}

#[test]
fn poll_tick_prefers_the_network_lane_then_store_while_it_runs() {
    let mut h = Harness::new(vec![account("1", true)]);
    h.fake.fetch_sets.lock().unwrap().clear();
    h.app.tick();
    h.app.tick();
    h.app.tick();
    h.settle();
    let sets = h.fake.fetch_sets.lock().unwrap().clone();
    assert_eq!(sets.iter().filter(|s| s.is_none()).count(), 1);
    assert!(sets.len() <= 2, "each lane is single-flight: {sets:?}");
}
