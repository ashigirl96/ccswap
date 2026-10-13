//! Launching the TUI. Ported from upstream
//! tests/test_tui.py::TestBareInvocation::{test_bare_tty_launches_tui,
//! test_bare_non_tty_keeps_usage_error}; the screens themselves are tested
//! in-crate against a fake surface.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{TestEnv, is_upstream, prog};

fn seeded() -> TestEnv {
    let env = TestEnv::new();
    env.seed_login("a@example.com", "", "");
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "sequence": [1],
        "accounts": {"1": {"email": "a@example.com", "organizationUuid": "", "organizationName": ""}},
    }));
    env
}

fn assert_dashboard_ran_and_quit(env: &TestEnv, args: &[&str]) {
    let out = env.cmd(args).run_in_terminal("q", Duration::from_secs(20));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("Switch account"), "{}", out.stdout);
    assert!(out.stdout.contains("a@example.com"), "{}", out.stdout);
    assert!(
        !env.store_dir()
            .join("cache/ccswap_update_check.json")
            .exists(),
        "the TUI exits before the update check"
    );
}

#[test]
fn bare_tty_launches_tui() {
    if is_upstream() {
        return;
    }
    assert_dashboard_ran_and_quit(&seeded(), &[]);
}

#[test]
fn tui_command_launches_tui() {
    if is_upstream() {
        return;
    }
    assert_dashboard_ran_and_quit(&seeded(), &["tui"]);
}

#[test]
fn bare_non_tty_keeps_usage_error() {
    let out = seeded().run::<[&str; 0], &str>([]);
    out.assert_code(2);
    assert!(
        out.stderr.ends_with(&format!(
            "error: no command given — try '{} help'\n",
            prog()
        )),
        "{}",
        out.stderr
    );
}

/// upstream TestBareInvocation::test_cswap_watch_opens_tui_on_watch_page:
/// the watch page sits over the dashboard, so `q` returns there first.
#[test]
fn watch_command_opens_the_watch_page_over_the_dashboard() {
    if is_upstream() {
        return;
    }
    let env = seeded();
    let out = env
        .cmd(["watch"])
        .run_in_terminal("qq", Duration::from_secs(20));
    assert_eq!(out.code, 0, "{}", out.stdout);
    let watch = out
        .stdout
        .find("watching all accounts")
        .expect("watch page");
    assert!(
        out.stdout[watch..].contains("Switch account"),
        "{}",
        out.stdout
    );
}

#[test]
fn auto_view_runs_the_engine_in_dry_run() {
    if is_upstream() {
        return;
    }
    let env = seeded();
    let out = env
        .cmd(["tui"])
        .run_in_terminal("gqq", Duration::from_secs(20));
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("DRY-RUN"), "{}", out.stdout);
    assert!(out.stdout.contains("engine started"), "{}", out.stdout);
}

const LIGHT_BACKGROUND: &str = "48;2;250;247;242m";

#[test]
fn auto_theme_follows_a_light_terminal_background() {
    if is_upstream() {
        return;
    }
    let env = seeded();
    let out = env.cmd(["tui"]).run_in_terminal_on(
        Some("rgb:ffff/ffff/ffff"),
        "q",
        Duration::from_secs(20),
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(
        out.stdout.contains("\x1b]11;?\x07"),
        "the background was asked"
    );
    assert!(out.stdout.contains(LIGHT_BACKGROUND), "{:?}", out.stdout);

    let dark = env.cmd(["tui"]).run_in_terminal_on(
        Some("rgb:0000/0000/0000"),
        "q",
        Duration::from_secs(20),
    );
    assert_eq!(dark.code, 0, "{}", dark.stdout);
    assert!(!dark.stdout.contains(LIGHT_BACKGROUND));
}

/// upstream main(): the CLI printer resolves `auto` the same way, and
/// `--json` output is never preceded by the query.
#[test]
fn cli_colours_follow_the_terminal_background_except_for_json() {
    if is_upstream() {
        return;
    }
    let env = seeded();
    let light_accent = "\x1b[38;2;149;76;42m";
    let out = env.cmd(["status"]).run_in_terminal_on(
        Some("rgb:ffff/ffff/ffff"),
        "",
        Duration::from_secs(20),
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains(light_accent), "{:?}", out.stdout);

    let json = env.cmd(["status", "--json"]).run_in_terminal_on(
        Some("rgb:ffff/ffff/ffff"),
        "",
        Duration::from_secs(20),
    );
    assert_eq!(json.code, 0, "{}", json.stdout);
    assert!(!json.stdout.contains("\x1b]11;?"), "{:?}", json.stdout);
}

#[test]
fn explicit_theme_wins_over_the_terminal_background() {
    if is_upstream() {
        return;
    }
    let env = seeded();
    env.write_json(
        ".claude-swap-backup/settings.json",
        &json!({"ui": {"theme": "dark"}}),
    );
    let out = env.cmd(["tui"]).run_in_terminal_on(
        Some("rgb:ffff/ffff/ffff"),
        "q",
        Duration::from_secs(20),
    );
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(!out.stdout.contains(LIGHT_BACKGROUND));
}
