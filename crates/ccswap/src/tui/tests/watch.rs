//! Ported from upstream tests/test_tui.py::TestWatchScreen.

use std::collections::HashSet;
use std::time::Instant;

use ccswap_core::usage::now_secs;

use crate::tui::app::{
    POLL_INTERVAL_S, SNAPSHOT_AGE_NOTE_S, Screen, WATCH_SELECT_TITLE, WatchScreen,
};
use crate::tui::testing::{Call, Gate, Harness, Lanes, account, entry_at, make_account};

fn two() -> Harness {
    Harness::new(vec![account("1", true), account("2", false)])
}

fn watch(h: &Harness) -> WatchScreen {
    match h.app.screen() {
        Screen::Watch(w) => w.clone(),
        other => panic!("watch screen expected, got {other:?}"),
    }
}

fn pct(h: &Harness) -> f64 {
    let usage = &h.app.snapshot.as_ref().unwrap().accounts[0].usage;
    usage.last_good.as_ref().unwrap()["five_hour"]["pct"]
        .as_f64()
        .unwrap()
}

#[test]
fn w_opens_monitor_without_cursor() {
    let mut h = two();
    h.press(&["w"]);
    let w = watch(&h);
    assert_eq!(w.list.numbers, ["1", "2"]);
    assert_eq!(w.list.index, None, "monitor mode: no cursor");
    let screen = h.render(100, 40);
    assert!(screen.contains("watching all accounts"));
    assert!(screen.contains(" 2  user2@example.com"));
    assert_eq!(
        screen.matches("5h").count(),
        2,
        "full cards for both: {screen}"
    );
    assert!(!screen.contains("┃"), "no threshold tick on watch cards");
    h.press(&["enter"]);
    h.settle();
    assert!(!h.calls().iter().any(|c| matches!(c, Call::SwitchTo(_))));
}

#[test]
fn s_arms_selection_switch_stays_watching() {
    let mut h = two();
    h.press(&["w", "s"]);
    assert_eq!(
        watch(&h).list.index,
        Some(0),
        "cursor on the active account"
    );
    assert!(h.render(100, 40).contains(WATCH_SELECT_TITLE));
    assert!(
        h.render(100, 40)
            .lines()
            .last()
            .unwrap()
            .contains("enter Confirm")
    );
    h.press(&["down", "enter"]);
    h.settle();
    assert!(h.calls().contains(&Call::SwitchTo("2".into())));
    let w = watch(&h);
    assert!(!w.selecting);
    assert_eq!(w.list.index, None);
    assert_eq!(
        h.app.snapshot.as_ref().unwrap().active_number.as_deref(),
        Some("2")
    );
}

#[test]
fn escape_disarms_then_leaves() {
    let mut h = two();
    h.press(&["w", "s", "escape"]);
    assert_eq!(watch(&h).list.index, None);
    h.press(&["escape"]);
    assert!(matches!(h.app.screen(), Screen::Dashboard(_)));
    assert!(!h.calls().iter().any(|c| matches!(c, Call::SwitchTo(_))));
}

#[test]
fn menu_watch_entry_opens_it() {
    let mut h = two();
    h.menu_select("watch");
    watch(&h);
}

#[test]
fn app_start_watch_stacks_over_dashboard() {
    let mut h = two();
    h.app.open_watch();
    watch(&h);
    h.press(&["escape"]);
    assert!(matches!(h.app.screen(), Screen::Dashboard(_)));
}

#[test]
fn rows_whose_usage_was_remeasured_flash() {
    let mut h = two();
    h.press(&["w"]);
    let now = Instant::now();
    assert!(
        !watch(&h).list.is_flashing("1", now),
        "not on the first snapshot"
    );
    h.fake.accounts.lock().unwrap()[1].usage = entry_at(now_secs(), 40.0);
    h.app.tick();
    h.settle();
    let w = watch(&h);
    assert!(w.list.is_flashing("2", Instant::now()));
    assert!(!w.list.is_flashing("1", Instant::now()));
    assert!(
        !w.list
            .is_flashing("2", Instant::now() + std::time::Duration::from_secs(2))
    );
}

#[test]
fn arrows_scroll_while_monitoring() {
    let accounts = (1..=8).map(|n| account(&n.to_string(), n == 1)).collect();
    let mut h = Harness::new(accounts);
    h.press(&["w"]);
    let before = h.render(100, 20);
    h.press(&["down", "down", "down", "down"]);
    assert_eq!(watch(&h).scroll, 4);
    assert_ne!(h.render(100, 20), before);
    h.press(&["up"]);
    assert_eq!(watch(&h).scroll, 3);
}

fn lanes(normal_pct: (f64, f64), store_pct: (f64, f64), block_store: bool) -> Lanes {
    Lanes {
        normal: vec![make_account(
            "1",
            true,
            entry_at(normal_pct.0, normal_pct.1),
        )],
        store: vec![make_account("1", true, entry_at(store_pct.0, store_pct.1))],
        normal_gate: Gate::new(false),
        store_gate: Gate::new(!block_store),
    }
}

fn lane_gates(h: &Harness) -> std::sync::Arc<Lanes> {
    h.fake.lanes.lock().unwrap().clone().unwrap()
}

#[test]
fn blocked_normal_allows_store_only_repaint_without_stale_overpaint() {
    let mut h = Harness::with_lanes(lanes((100.0, 10.0), (200.0, 80.0), false));
    h.wait_for_passes(1);
    h.app.tick();
    h.wait_until(|app| app.snapshot.is_some());
    assert_eq!(pct(&h), 80.0);
    lane_gates(&h).normal_gate.open();
    h.settle();
    assert_eq!(pct(&h), 80.0);
    assert_eq!(h.fetch_sets(), [None, Some(HashSet::new())]);
}

#[test]
fn late_normal_can_advance_usage_after_store_repaint() {
    let mut h = Harness::with_lanes(lanes((200.0, 80.0), (100.0, 10.0), false));
    h.wait_for_passes(1);
    h.app.tick();
    h.wait_until(|app| app.snapshot.is_some());
    assert_eq!(pct(&h), 10.0);
    lane_gates(&h).normal_gate.open();
    h.settle();
    assert_eq!(pct(&h), 80.0);
}

#[test]
fn repeated_ticks_keep_store_lane_single_flight() {
    let mut h = Harness::with_lanes(lanes((100.0, 10.0), (200.0, 80.0), true));
    h.wait_for_passes(1);
    h.app.tick();
    h.wait_for_passes(2);
    h.app.tick();
    h.app.tick();
    assert_eq!(h.fetch_sets(), [None, Some(HashSet::new())]);
    let gates = lane_gates(&h);
    gates.store_gate.open();
    gates.normal_gate.open();
    h.settle();
}

#[test]
fn store_only_mode_launches_only_store_lane() {
    let mut h = two();
    h.fake.fetch_sets.lock().unwrap().clear();
    h.app.set_store_only(true);
    h.settle();
    assert_eq!(h.fetch_sets(), [Some(HashSet::new())]);
    h.app.tick();
    h.settle();
    assert!(h.fetch_sets().iter().all(Option::is_some));
}

#[test]
fn watch_title_shows_snapshot_age_and_long_refresh() {
    let mut h = two();
    h.press(&["w"]);
    let title = |h: &Harness| h.render(100, 40).lines().nth(1).unwrap().to_string();
    assert!(
        !title(&h).contains("snapshot"),
        "fresh snapshots stay quiet"
    );
    h.app.snapshot.as_mut().unwrap().taken_at = now_secs() - SNAPSHOT_AGE_NOTE_S - 1.0;
    h.app.update_refresh_status();
    assert!(title(&h).contains("watching all accounts · snapshot 1m ago"));
    h.app
        .pretend_normal_refresh_running_for(POLL_INTERVAL_S + 1.0);
    h.app.update_refresh_status();
    assert!(title(&h).contains("refreshing"), "{}", title(&h));
}
