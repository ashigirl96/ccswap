mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::json;
use support::{CodexAuth, TestEnv};

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn add_registers_the_live_login_and_stores_its_bytes() {
    let env = TestEnv::new();
    let live = env.seed_codex_login(&CodexAuth::new("A@Example.com", "ACCT-A").plan("pro"));

    let out = env.run(["codex", "add"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("Added Codex account 1"),
        "{}",
        out.stdout
    );
    let ledger = env.codex_ledger();
    assert_eq!(ledger["sequence"], json!([1]));
    let row = &ledger["accounts"]["1"];
    assert_eq!(row["email"], "a@example.com");
    assert_eq!(row["account_id"], "acct-a");
    assert_eq!(row["plan"], "pro");
    assert_eq!(row["alias"], json!(null));
    assert!(row["added"].as_str().unwrap().ends_with('Z'), "{row}");
    assert_eq!(env.codex_managed_auth(1).as_deref(), Some(live.as_str()));
    assert_eq!(mode(&env.codex_managed_home(1)), 0o700);
    assert_eq!(mode(&env.codex_managed_home(1).join("auth.json")), 0o600);
    assert_eq!(mode(&env.codex_ledger_path()), 0o600);
}

#[test]
fn adding_the_same_account_twice_names_the_existing_number_and_adds_nothing() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("other@example.com", "acct-other"));
    let stored = env.seed_codex_account(2, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();
    env.seed_codex_login(&CodexAuth::new("a@example.com", "ACCT-A").tag("v2"));

    let out = env.run(["codex", "add"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("already managed as 2"),
        "{}",
        out.stdout
    );
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
    assert_eq!(env.codex_managed_auth(2), Some(stored));
    assert!(!env.codex_managed_home(3).exists());
}

#[test]
fn the_same_email_in_another_workspace_is_a_separate_account() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-personal"));
    let work = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-work").plan("team"));

    let out = env.run(["codex", "add"]);

    out.assert_code(0);
    let ledger = env.codex_ledger();
    assert_eq!(ledger["sequence"], json!([1, 2]));
    assert_eq!(ledger["accounts"]["2"]["account_id"], "acct-work");
    assert_eq!(ledger["accounts"]["2"]["email"], "a@example.com");
    assert_eq!(env.codex_managed_auth(2), Some(work));
}

#[test]
fn the_account_id_falls_back_to_the_id_token_claim() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_login(&CodexAuth::new("a@example.com", "ACCT-A").without_token_account_id());

    let out = env.run(["codex", "add"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("already managed as 1"),
        "{}",
        out.stdout
    );
}

#[test]
fn add_reads_the_live_login_under_codex_home() {
    let mut env = TestEnv::new();
    let home = env.use_custom_codex_home();
    let live = env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    assert!(env.live_codex_auth_path().starts_with(&home));

    env.run(["codex", "add"]).assert_code(0);

    assert_eq!(env.codex_managed_auth(1), Some(live));
}

#[test]
fn add_without_a_live_login_fails_and_says_where_it_looked() {
    let env = TestEnv::new();

    let out = env.run(["codex", "add"]);

    out.assert_code(1);
    let expected = env.live_codex_auth_path().display().to_string();
    assert!(
        out.stderr.contains("No Codex login found at"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains(&expected), "{}", out.stderr);
    assert!(!env.codex_store_dir().exists());
}

#[test]
fn add_refuses_an_api_key_login() {
    let env = TestEnv::new();
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a").api_key_only());

    let out = env.run(["codex", "add"]);

    out.assert_code(1);
    assert!(out.stderr.contains("no ChatGPT tokens"), "{}", out.stderr);
    assert!(!env.codex_store_dir().exists());
}

#[test]
fn add_waits_for_the_store_lock_before_writing_the_ledger() {
    use std::os::fd::AsRawFd;

    let env = TestEnv::new();
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    std::fs::create_dir_all(env.store_dir()).unwrap();
    let lock = std::fs::File::create(env.store_dir().join(".lock")).unwrap();
    // SAFETY: flock on a descriptor we own.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let running = env.cmd(["codex", "add"]).spawn();
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert!(
        !env.codex_ledger_path().exists(),
        "wrote the ledger while another process held .lock"
    );
    drop(lock);
    let out = running.finish(std::time::Duration::from_secs(10));

    out.assert_code(0);
    assert_eq!(env.codex_ledger()["sequence"], json!([1]));
}

#[test]
fn codex_commands_leave_the_claude_store_and_keychain_alone() {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "sequence": [1],
        "accounts": { "1": { "email": "claude@example.com", "uuid": "u-1" } },
    }));
    env.security().set_item(
        support::ACTIVE_OAUTH_SERVICE,
        support::KEYCHAIN_ACCOUNT,
        "{}",
    );
    let sequence_before = std::fs::read(env.store_dir().join("sequence.json")).unwrap();
    let store_entries = |env: &TestEnv| {
        let mut names: Vec<String> = std::fs::read_dir(env.store_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "codex" && n != ".lock")
            .collect();
        names.sort();
        names
    };
    let entries_before = store_entries(&env);
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));

    env.run(["codex", "add"]).assert_code(0);
    env.run(["codex", "list"]).assert_code(0);
    env.run(["codex", "list", "--json"]).assert_code(0);
    env.codex()
        .script_login(Some(&CodexAuth::new("b@example.com", "acct-b").render()), 0);
    env.run(["codex", "login"]).assert_code(0);
    env.run(["codex", "switch", "2"]).assert_code(0);
    env.run(["codex", "switch", "--json"]).assert_code(0);

    assert_eq!(
        std::fs::read(env.store_dir().join("sequence.json")).unwrap(),
        sequence_before
    );
    assert_eq!(store_entries(&env), entries_before);
    assert!(
        env.security().calls().is_empty(),
        "{:?}",
        env.security().calls()
    );
    assert!(env.http().requests().is_empty());
}
