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
