mod support;

use std::os::unix::fs::PermissionsExt;

use serde_json::json;
use support::{CodexAuth, TestEnv};

fn live(env: &TestEnv) -> String {
    std::fs::read_to_string(env.live_codex_auth_path()).unwrap()
}

fn live_mode(env: &TestEnv) -> u32 {
    std::fs::metadata(env.live_codex_auth_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn switch_by_number_makes_the_target_live_and_writes_the_outgoing_login_back() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let b = env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let refreshed_a = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a").tag("v2"));

    let out = env.run(["codex", "switch", "2"]);

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("Switched to Codex account 2 (b@example.com)"),
        "{}",
        out.stdout
    );
    assert_eq!(live(&env), b);
    assert_eq!(live_mode(&env), 0o600);
    assert_eq!(env.codex_managed_auth(1), Some(refreshed_a));
    assert_eq!(env.codex_managed_auth(2), Some(b));
    assert_eq!(env.codex_ledger()["active"], json!("2"));
}

#[test]
fn switch_by_email_or_alias_picks_that_account() {
    let env = TestEnv::new();
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let b = env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));

    env.run(["codex", "switch", "B@example.com"]).assert_code(0);
    assert_eq!(live(&env), b);

    let mut ledger = env.codex_ledger();
    ledger["accounts"]["1"]["alias"] = json!("home");
    env.seed_codex_ledger(ledger);
    env.run(["codex", "switch", "home"]).assert_code(0);
    assert_eq!(env.codex_managed_auth(1), Some(live(&env)));
}

#[test]
fn bare_switch_rotates_to_the_next_account_in_sequence() {
    let env = TestEnv::new();
    let a = env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let b = env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let c = env.seed_codex_account(3, &CodexAuth::new("c@example.com", "acct-c"));
    env.seed_live_codex_auth(&b);

    env.run(["codex", "switch"]).assert_code(0);
    assert_eq!(live(&env), c);

    env.run(["codex", "switch"]).assert_code(0);
    assert_eq!(live(&env), a);
}

#[test]
fn ambiguous_or_unknown_identifiers_are_errors_and_change_nothing() {
    let env = TestEnv::new();
    let live_a = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b-personal"));
    env.seed_codex_account(
        3,
        &CodexAuth::new("b@example.com", "acct-b-work").plan("team"),
    );
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();

    let out = env.run(["codex", "switch", "b@example.com"]);
    out.assert_code(1);
    assert!(
        out.stderr.contains(
            "Email 'b@example.com' is ambiguous — matches Codex accounts: 2 [plus], 3 [team]"
        ),
        "{}",
        out.stderr
    );

    let out = env.run(["codex", "switch", "9"]);
    out.assert_code(1);
    assert!(
        out.stderr.contains("Codex account 9 does not exist"),
        "{}",
        out.stderr
    );

    let out = env.run(["codex", "switch", "nobody@example.com"]);
    out.assert_code(1);
    assert!(
        out.stderr
            .contains("No Codex account found with identifier: nobody@example.com"),
        "{}",
        out.stderr
    );

    assert_eq!(live(&env), live_a);
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

#[test]
fn an_unmanaged_live_login_is_set_aside_in_unclaimed() {
    let env = TestEnv::new();
    let a = env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let stranger = env.seed_codex_login(&CodexAuth::new("x@example.com", "acct-x"));

    let out = env.run(["codex", "switch", "1"]);

    out.assert_code(0);
    assert_eq!(live(&env), a);
    let stashed: Vec<String> = std::fs::read_dir(env.codex_store_dir().join("unclaimed"))
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(stashed, vec![stranger]);
    assert!(
        out.stdout.contains("not a managed account"),
        "{}",
        out.stdout
    );
}

#[test]
fn a_live_login_rewritten_mid_switch_is_saved_again_before_it_is_replaced() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let b = env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a").tag("v2"));
    let refreshed = CodexAuth::new("a@example.com", "acct-a").tag("v3").render();
    env.codex().rewrite_live_during_switch(&refreshed);
    let (hook, program) = env.codex().switch_hook_env();

    let out = env.cmd(["codex", "switch", "2"]).env(hook, &program).run();

    out.assert_code(0);
    assert_eq!(env.codex_managed_auth(1), Some(refreshed));
    assert_eq!(live(&env), b);
}

#[test]
fn a_live_login_that_keeps_changing_fails_the_switch() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    for n in 0..10 {
        let auth = CodexAuth::new("a@example.com", "acct-a").tag(&format!("r{n}"));
        env.codex().rewrite_live_during_switch(&auth.render());
    }
    let (hook, program) = env.codex().switch_hook_env();

    let out = env.cmd(["codex", "switch", "2"]).env(hook, &program).run();

    out.assert_code(1);
    assert!(out.stderr.contains("kept changing"), "{}", out.stderr);
    assert_eq!(env.codex_ledger()["active"], json!(null));
    assert!(live(&env).contains("acct-a"));
}

#[test]
fn a_non_file_credentials_store_refuses_the_switch_and_writes_nothing() {
    for mode in ["keyring", "auto", "ephemeral"] {
        let env = TestEnv::new();
        env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
        env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
        let live_before =
            env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a").tag("v2"));
        env.write_file(
            env.codex_home().join("config.toml"),
            &format!("model = \"o3\"\ncli_auth_credentials_store = \"{mode}\"\n"),
        );
        let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
        let home_before = env.codex_managed_auth(1);

        let out = env.run(["codex", "switch", "2"]);

        out.assert_code(1);
        assert!(
            out.stderr.contains(&format!("'{mode}'"))
                && out.stderr.contains("cli_auth_credentials_store"),
            "{mode}: {}",
            out.stderr
        );
        assert_eq!(live(&env), live_before);
        assert_eq!(env.codex_managed_auth(1), home_before);
        assert_eq!(
            std::fs::read(env.codex_ledger_path()).unwrap(),
            ledger_before
        );
        assert!(!env.codex_store_dir().join("unclaimed").exists());
    }
}

#[test]
fn a_file_credentials_store_or_one_scoped_to_a_profile_allows_the_switch() {
    for config in [
        "cli_auth_credentials_store = \"file\"\n",
        "[profiles.work]\ncli_auth_credentials_store = \"keyring\"\n",
    ] {
        let env = TestEnv::new();
        env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
        let b = env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
        env.write_file(env.codex_home().join("config.toml"), config);

        env.run(["codex", "switch", "2"]).assert_code(0);

        assert_eq!(live(&env), b, "{config}");
    }
}

#[test]
fn switching_to_the_live_account_does_nothing() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let refreshed = env.seed_codex_login(&CodexAuth::new("b@example.com", "acct-b").tag("v2"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    let home_before = env.codex_managed_auth(2);

    let out = env.run(["codex", "switch", "2"]);

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("Already on Codex account 2 (b@example.com)"),
        "{}",
        out.stdout
    );
    assert_eq!(live(&env), refreshed);
    assert_eq!(env.codex_managed_auth(2), home_before);
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

#[test]
fn the_restart_hint_appears_only_when_the_app_server_daemon_directory_exists() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));

    let out = env.run(["codex", "switch", "2"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("codex sessions started from now on"),
        "{}",
        out.stdout
    );
    assert!(!out.stdout.contains("daemon restart"), "{}", out.stdout);

    std::fs::create_dir_all(env.codex_home().join("app-server-daemon")).unwrap();
    let out = env.run(["codex", "switch", "1"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("codex sessions started from now on"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("codex app-server daemon restart"),
        "{}",
        out.stdout
    );
    assert!(
        env.codex().calls().is_empty(),
        "ccswap must not run codex itself: {:?}",
        env.codex().calls()
    );
}

#[test]
fn switch_json_uses_the_claude_switch_envelope() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));

    let out = env.run(["codex", "switch", "2", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1,
            "switched": true,
            "from": { "number": 1, "email": "a@example.com" },
            "to": { "number": 2, "email": "b@example.com" },
            "strategy": "direct",
            "reason": "switched",
            "message": "Switched to Codex account 2 (b@example.com)",
            "warnings": [],
        })
    );

    let out = env.run(["codex", "switch", "--json"]);
    out.assert_code(0);
    let report = out.json();
    assert_eq!(report["strategy"], "rotation");
    assert_eq!(
        report["to"],
        json!({ "number": 1, "email": "a@example.com" })
    );

    let out = env.run(["codex", "switch", "1", "--json"]);
    out.assert_code(0);
    let report = out.json();
    assert_eq!(report["switched"], false);
    assert_eq!(report["reason"], "already-active");

    let out = env.run(["codex", "switch", "9", "--json"]);
    out.assert_code(1);
    assert_eq!(out.json()["error"]["type"], "AccountNotFoundError");
}
