mod support;

use serde_json::json;
use support::{CodexAuth, TestEnv};

fn two_accounts_with_b_live(env: &TestEnv) {
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a").plan("plus"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b").plan("team"));
    let mut ledger = env.codex_ledger();
    ledger["accounts"]["2"]["alias"] = "work".into();
    env.seed_codex_ledger(ledger);
    env.seed_codex_login(&CodexAuth::new("b@example.com", "acct-b").tag("refreshed"));
}

#[test]
fn list_shows_number_email_alias_plan_and_marks_the_live_account() {
    let env = TestEnv::new();
    two_accounts_with_b_live(&env);

    let out = env.run(["codex", "list"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Codex accounts:\n  1: a@example.com [plus]\n  2: work (b@example.com) [team] (active)\n"
    );
}

#[test]
fn list_json_uses_the_claude_envelope() {
    let env = TestEnv::new();
    two_accounts_with_b_live(&env);

    let out = env.run(["codex", "ls", "--json"]);

    out.assert_code(0);
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1,
            "activeAccountNumber": 2,
            "accounts": [
                { "number": 1, "email": "a@example.com", "accountId": "acct-a", "plan": "plus", "active": false },
                { "number": 2, "email": "b@example.com", "accountId": "acct-b", "plan": "team", "active": true, "alias": "work" },
            ],
        })
    );
}

#[test]
fn an_unregistered_live_login_marks_nothing_active() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_login(&CodexAuth::new("z@example.com", "acct-z"));

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["activeAccountNumber"], json!(null));
    assert_eq!(payload["accounts"][0]["active"], false);
}

#[test]
fn an_empty_store_lists_nothing_and_points_at_add() {
    let env = TestEnv::new();

    let out = env.run(["codex", "list"]);
    out.assert_code(0);
    assert!(out.stdout.contains("ccswap codex add"), "{}", out.stdout);

    let out = env.run(["codex", "list", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.json(),
        json!({ "schemaVersion": 1, "activeAccountNumber": null, "accounts": [] })
    );
    assert!(!env.codex_store_dir().exists());
}

#[test]
fn a_broken_ledger_is_a_json_error_envelope() {
    let env = TestEnv::new();
    env.write_file(env.codex_ledger_path(), "{not json");

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(1);
    let payload = out.json();
    assert_eq!(payload["error"]["type"], "ConfigError");
}
