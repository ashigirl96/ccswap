//! Ported from upstream tests/test_tui.py::{TestAutoScreen, TestThemeWiring}.

use serde_json::{Value, json};

use crate::tui::app::{AppConfig, AutoScreen, Screen};
use crate::tui::testing::{Harness, account, entry, make_account};
use crate::tui::theme::{DARK, LIGHT, ThemeName};

fn two() -> Harness {
    Harness::new(vec![account("1", true), account("2", false)])
}

fn open(h: &mut Harness) {
    h.press(&["g"]);
    h.wait_until(|app| app.auto_screen().is_some_and(|a| a.log.len() > 1));
}

fn auto(h: &Harness) -> AutoScreen {
    h.app.auto_screen().expect("auto screen").clone()
}

fn engine<T>(h: &Harness, i: usize, f: impl FnOnce(&crate::tui::testing::FakeEngine) -> T) -> T {
    h.engines.with(|e| f(&e[i]))
}

fn engines(h: &Harness) -> usize {
    h.engines.with(<[_]>::len)
}

fn summary(h: &Harness) -> String {
    h.render(100, 40)
        .lines()
        .find(|l| l.contains("auto-switch ·"))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn opens_in_dry_run_and_store_only() {
    let mut h = two();
    open(&mut h);
    assert!(matches!(h.app.screen(), Screen::Auto(_)));
    assert_eq!(engines(&h), 1);
    assert!(engine(&h, 0, |e| e.dry_run));
    assert!(h.app.store_only());
    let log: Vec<String> = auto(&h).log.iter().map(|l| l.plain()).collect();
    assert_eq!(log[0], "— engine started: DRY-RUN (watching only) —");
    assert!(log[1].ends_with("no switch: cooldown"), "{log:?}");
    let screen = h.render(100, 40);
    assert!(screen.contains(" DRY-RUN "));
    assert!(screen.contains("auto-switch · threshold 90% · poll every 60s"));
    assert!(screen.contains("no switch: cooldown"));
}

#[test]
fn go_live_requires_confirmation() {
    let mut h = two();
    open(&mut h);
    h.press(&["l"]);
    let Screen::Confirm(c) = h.app.screen() else {
        panic!("confirm modal expected");
    };
    assert_eq!(c.title, "Go live");
    assert!(c.message.starts_with("Go live? claude-swap will switch"));
    h.press(&["y"]);
    assert_eq!(engines(&h), 2);
    assert!(engine(&h, 0, |e| e.stopped));
    assert!(!engine(&h, 1, |e| e.dry_run));
    assert!(h.render(100, 40).contains(" LIVE "));
    assert!(
        auto(&h)
            .log
            .iter()
            .any(|l| l.text == "— engine started: LIVE (will switch accounts) —")
    );
    h.press(&["l"]);
    assert_eq!(engines(&h), 3, "live to dry-run needs no confirmation");
    assert!(engine(&h, 2, |e| e.dry_run));
}

#[test]
fn declining_go_live_keeps_the_dry_run_engine() {
    let mut h = two();
    open(&mut h);
    h.press(&["l", "n"]);
    assert_eq!(engines(&h), 1);
    assert!(!engine(&h, 0, |e| e.stopped));
}

#[test]
fn back_stops_engine_and_restores_fetching() {
    let mut h = two();
    open(&mut h);
    // Workers record their lane when they run, so passes still in flight would land in any order.
    h.settle();
    let before = h.fetch_sets().len();
    h.press(&["escape"]);
    h.settle();
    assert!(matches!(h.app.screen(), Screen::Dashboard(_)));
    assert!(engine(&h, 0, |e| e.stopped));
    assert!(!h.app.store_only());
    assert_eq!(h.fetch_sets()[before..], [None]);
}

#[test]
fn threshold_adjust_is_session_only() {
    let mut h = two();
    open(&mut h);
    assert_eq!(
        h.app.threshold_pct,
        Some(90.0),
        "mount syncs to the file value"
    );
    h.press(&["right"]);
    assert_eq!(
        auto(&h).settings.threshold,
        90.0,
        "inert outside adjust mode"
    );
    h.press(&["t", "right", "right", "right"]);
    assert_eq!(auto(&h).settings.threshold, 93.0);
    assert_eq!(h.app.threshold_pct, Some(93.0));
    assert_eq!(
        engine(&h, 0, |e| e.applied_thresholds.clone()),
        [91.0, 92.0, 93.0]
    );
    assert_eq!(
        h.fake.poll_inputs.lock().unwrap().clone(),
        Some((93.0, Vec::new()))
    );
    assert!(summary(&h).contains("threshold 93% (session)"));
    assert!(summary(&h).contains("← → adjust · enter done"));
    h.press(&["enter"]);
    assert_eq!(
        engine(&h, 0, |e| e.wakes),
        1,
        "one forced tick on leaving the mode"
    );
    assert_eq!(
        auto(&h).log.last().unwrap().text,
        "— threshold set to 93% for this session —"
    );
    assert!(!h.backup_dir().join("settings.json").exists());
    h.press(&["l", "y"]);
    assert_eq!(engine(&h, 1, |e| e.settings.threshold), 93.0);
    h.press(&["escape"]);
    h.settle();
    assert_eq!(h.app.threshold_pct, Some(90.0));
    assert_eq!(*h.fake.poll_inputs.lock().unwrap(), None);
}

#[test]
fn threshold_adjust_escape_exits_mode_not_screen() {
    let mut h = two();
    open(&mut h);
    h.press(&["t", "escape"]);
    assert!(matches!(h.app.screen(), Screen::Auto(_)));
    assert_eq!(
        engine(&h, 0, |e| e.wakes),
        0,
        "no net change, no forced tick"
    );
    h.press(&["escape"]);
    assert!(matches!(h.app.screen(), Screen::Dashboard(_)));
}

#[test]
fn threshold_clamps_and_keeps_meaningful_decimals() {
    let mut h = Harness::with_settings(
        vec![account("1", true), account("2", false)],
        Some(json!({"schemaVersion": 1, "autoswitch": {"threshold": 99.0}})),
    );
    open(&mut h);
    h.press(&["t", "right", "right"]);
    assert_eq!(auto(&h).settings.threshold, 99.9);
    assert!(
        summary(&h).contains("threshold 99.9% (session)"),
        "never a lying 100%"
    );
    h.app.threshold_step(-60.0);
    assert_eq!(auto(&h).settings.threshold, 50.0);
}

fn candidates(h: &Harness) -> String {
    let screen = h.render(100, 40);
    let start = screen.find("Next best").expect("candidates");
    screen[start..].to_string()
}

#[test]
fn candidates_ranked_by_headroom() {
    let mut h = Harness::new(vec![
        make_account("1", true, entry(Some(91.0), Some(20.0))),
        make_account("2", false, entry(Some(80.0), Some(10.0))),
        make_account("3", false, entry(Some(15.0), Some(5.0))),
    ]);
    open(&mut h);
    let text = candidates(&h);
    assert!(text.find("user3@example.com").unwrap() < text.find("user2@example.com").unwrap());
    assert!(text.contains(" 15% used"));
    assert!(
        !text.contains("user1@example.com"),
        "the active account is no candidate"
    );
}

#[test]
fn candidates_ranking_honors_configured_model() {
    let scoped = |pct5, model_pct| {
        let mut e = entry(Some(pct5), Some(5.0));
        e.last_good.as_mut().unwrap()["scoped"] = json!([{"name": "Fable", "pct": model_pct}]);
        e
    };
    let mut h = Harness::with_settings(
        vec![
            make_account("1", true, entry(Some(91.0), Some(20.0))),
            make_account("2", false, scoped(10.0, 95.0)),
            make_account("3", false, scoped(50.0, 20.0)),
        ],
        Some(json!({"schemaVersion": 1, "autoswitch": {"model": "Fable"}})),
    );
    open(&mut h);
    let text = candidates(&h);
    assert!(text.find("user3@example.com").unwrap() < text.find("user2@example.com").unwrap());
}

#[test]
fn no_candidates_says_so() {
    let mut h = Harness::new(vec![account("1", true)]);
    open(&mut h);
    assert!(candidates(&h).contains("no other switchable accounts"));
}

#[test]
fn menu_auto_entry_opens_it() {
    let mut h = two();
    h.menu_select("auto");
    assert!(matches!(h.app.screen(), Screen::Auto(_)));
}

fn themed(settings: Value, detected_light: Option<bool>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), settings.to_string()).unwrap();
    let mut config = AppConfig::load(&ccswap_core::settings::Settings::in_dir(dir.path()));
    config.detected_light = detected_light;
    Harness::with_config(vec![account("1", true)], config)
}

#[test]
fn mount_selects_light_theme_from_settings() {
    let h = themed(json!({"ui": {"theme": "light"}}), None);
    assert_eq!(h.app.palette(), LIGHT);
}

#[test]
fn auto_setting_uses_detected_light_else_dark() {
    let auto = json!({"ui": {"theme": "auto"}});
    assert_eq!(themed(auto.clone(), Some(true)).app.palette(), LIGHT);
    assert_eq!(themed(auto.clone(), Some(false)).app.palette(), DARK);
    assert_eq!(themed(auto, None).app.palette(), DARK);
}

#[test]
fn toggle_cycles_dark_light_auto_and_saves() {
    let mut h = themed(json!({"ui": {"theme": "dark"}}), Some(true));
    let saved = |h: &Harness| {
        let text = std::fs::read_to_string(h.backup_dir().join("settings.json")).unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["ui"]["theme"].clone()
    };
    assert_eq!(h.app.palette(), DARK);
    h.press(&["ctrl+t"]);
    assert_eq!(h.app.palette(), LIGHT);
    h.press(&["ctrl+t"]);
    assert_eq!(h.app.theme_name, ThemeName::Auto);
    assert_eq!(
        h.app.palette(),
        LIGHT,
        "auto follows the detected background"
    );
    assert_eq!(saved(&h), "auto");
    h.press(&["ctrl+t"]);
    assert_eq!(h.app.palette(), DARK);
    assert_eq!(saved(&h), "dark");
}

#[test]
fn theme_menu_defaults_to_auto_and_applies() {
    let mut h = Harness::with_settings(vec![account("1", true)], None);
    assert_eq!(h.app.theme_name, ThemeName::Auto);
    h.menu_select("theme-menu");
    let labels = h.menu_labels();
    assert!(labels.iter().any(|l| l == "● auto"), "{labels:?}");
    h.menu_select("theme:light");
    assert_eq!(h.app.palette(), LIGHT);
}

#[test]
fn render_uses_the_selected_palette() {
    let mut h = themed(json!({"ui": {"theme": "light"}}), None);
    h.press(&["g"]);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
    terminal
        .draw(|f| crate::tui::view::draw(f, &h.app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(0, 0)].bg, LIGHT.background);
}
