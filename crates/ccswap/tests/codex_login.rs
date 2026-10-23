mod support;

use serde_json::json;
use support::{CodexAuth, Output, TestEnv};

#[test]
fn login_registers_the_new_account_under_a_new_number() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let new = CodexAuth::new("B@Example.com", "ACCT-B")
        .plan("plus")
        .render();
    env.codex().script_login(Some(&new), 0);

    let out = env.run(["codex", "login"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("Added Codex account 2"),
        "{}",
        out.stdout
    );
    let ledger = env.codex_ledger();
    assert_eq!(ledger["sequence"], json!([1, 2]));
    assert_eq!(ledger["accounts"]["2"]["email"], "b@example.com");
    assert_eq!(ledger["accounts"]["2"]["account_id"], "acct-b");
    assert_eq!(ledger["accounts"]["2"]["plan"], "plus");
    assert_eq!(env.codex_managed_auth(2).as_deref(), Some(new.as_str()));
}

#[test]
fn codex_login_runs_in_the_new_managed_home_created_beforehand() {
    let mut env = TestEnv::new();
    env.use_custom_codex_home();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.codex()
        .script_login(Some(&CodexAuth::new("b@example.com", "acct-b").render()), 0);

    env.run(["codex", "login"]).assert_code(0);

    let calls = env.codex().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].argv, ["login"]);
    assert_eq!(
        calls[0].codex_home.as_deref(),
        Some(env.codex_managed_home(2).to_str().unwrap())
    );
    assert!(calls[0].codex_home_existed, "{calls:?}");
}

#[test]
fn logging_in_to_a_managed_account_updates_its_stored_login_and_drops_the_new_home() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    let fresh = CodexAuth::new("b@example.com", "ACCT-B").tag("v2").render();
    env.codex().script_login(Some(&fresh), 0);

    let out = env.run(["codex", "login"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("already managed as 2"),
        "{}",
        out.stdout
    );
    assert_eq!(env.codex_managed_auth(2).as_deref(), Some(fresh.as_str()));
    assert!(!env.codex_managed_home(3).exists());
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

fn assert_login_left_nothing(env: &TestEnv, out: &Output, ledger_before: &[u8]) {
    out.assert_code(1);
    assert!(out.stderr.contains("Error:"), "{}", out.stderr);
    assert!(!env.codex_managed_home(2).exists());
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

#[test]
fn a_failed_codex_login_removes_the_new_home_and_leaves_the_ledger() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    env.codex().script_login(None, 1);

    let out = env.run(["codex", "login"]);

    assert_login_left_nothing(&env, &out, &ledger_before);
    assert!(out.stderr.contains("exited with 1"), "{}", out.stderr);
}

#[test]
fn an_interrupted_codex_login_removes_the_new_home_even_if_it_wrote_auth() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    env.codex().script_login(
        Some(&CodexAuth::new("b@example.com", "acct-b").render()),
        130,
    );

    let out = env.run(["codex", "login"]);

    assert_login_left_nothing(&env, &out, &ledger_before);
}

#[test]
fn a_codex_login_that_wrote_no_chatgpt_tokens_adds_nothing() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    env.codex().script_login(None, 0);
    env.codex().script_login(
        Some(
            &CodexAuth::new("b@example.com", "acct-b")
                .api_key_only()
                .render(),
        ),
        0,
    );

    let no_auth = env.run(["codex", "login"]);
    assert_login_left_nothing(&env, &no_auth, &ledger_before);
    let api_key = env.run(["codex", "login"]);
    assert_login_left_nothing(&env, &api_key, &ledger_before);
}

#[test]
fn login_leaves_the_live_auth_json_alone() {
    let mut env = TestEnv::new();
    env.use_custom_codex_home();
    let live = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    env.run(["codex", "add"]).assert_code(0);
    let ledger_active = env.codex_ledger()["active"].clone();
    env.codex()
        .script_login(Some(&CodexAuth::new("b@example.com", "acct-b").render()), 0);

    env.run(["codex", "login"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(env.live_codex_auth_path()).unwrap(),
        live
    );
    assert_eq!(env.codex_ledger()["active"], ledger_active);
}

#[test]
fn login_skips_a_number_whose_home_is_already_taken() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    std::fs::create_dir_all(env.codex_managed_home(2)).unwrap();
    env.codex()
        .script_login(Some(&CodexAuth::new("b@example.com", "acct-b").render()), 0);

    env.run(["codex", "login"]).assert_code(0);

    assert_eq!(env.codex_ledger()["sequence"], json!([1, 3]));
    assert!(env.codex_managed_auth(3).is_some());
}
