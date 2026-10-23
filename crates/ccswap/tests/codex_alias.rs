mod support;

use serde_json::json;
use support::{CodexAuth, TestEnv};

#[test]
fn an_alias_shows_in_list_and_list_json() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));

    let out = env.run(["codex", "alias", "b@example.com", "Work"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("Codex account 2 as work"),
        "{}",
        out.stdout
    );
    assert_eq!(env.codex_ledger()["accounts"]["2"]["alias"], "work");
    let list = env.run(["codex", "list"]);
    assert!(
        list.stdout.contains("2: work (b@example.com)"),
        "{}",
        list.stdout
    );
    let rows = env.run(["codex", "list", "--json"]).json()["accounts"].clone();
    assert_eq!(rows[1]["alias"], "work");
    assert!(rows[0].get("alias").is_none(), "{rows}");
}

#[test]
fn an_empty_name_removes_the_alias() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.run(["codex", "alias", "1", "work"]).assert_code(0);

    let out = env.run(["codex", "alias", "work", ""]);

    out.assert_code(0);
    assert_eq!(env.codex_ledger()["accounts"]["1"]["alias"], json!(null));
    let rows = env.run(["codex", "list", "--json"]).json()["accounts"].clone();
    assert!(rows[0].get("alias").is_none(), "{rows}");
}

#[test]
fn unset_removes_the_alias_as_the_claude_alias_does() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.run(["codex", "alias", "1", "work"]).assert_code(0);

    let out = env.run(["codex", "alias", "work", "--unset"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("the alias of Codex account 1"),
        "{}",
        out.stdout
    );
    assert_eq!(env.codex_ledger()["accounts"]["1"]["alias"], json!(null));
}

#[test]
fn unset_takes_an_account_and_no_name() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.run(["codex", "alias", "1", "work"]).assert_code(0);

    let with_name = env.run(["codex", "alias", "1", "home", "--unset"]);
    with_name.assert_code(1);
    assert!(
        with_name.stderr.contains("--unset does not take a name"),
        "{}",
        with_name.stderr
    );
    let without_account = env.run(["codex", "alias", "--unset"]);
    without_account.assert_code(1);
    assert!(
        without_account.stderr.contains("needs an account"),
        "{}",
        without_account.stderr
    );
    assert_eq!(env.codex_ledger()["accounts"]["1"]["alias"], "work");
}

#[test]
fn an_alias_follows_the_claude_alias_rules_and_stays_unique() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    env.run(["codex", "alias", "1", "work"]).assert_code(0);
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();

    let numeric = env.run(["codex", "alias", "2", "42"]);
    numeric.assert_code(1);
    assert!(
        numeric.stderr.contains("cannot be purely numeric"),
        "{}",
        numeric.stderr
    );
    let taken = env.run(["codex", "alias", "2", "WORK"]);
    taken.assert_code(1);
    assert!(
        taken
            .stderr
            .contains("Alias 'work' is already used by Codex account 1"),
        "{}",
        taken.stderr
    );
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

#[test]
fn alias_needs_both_an_account_and_a_name() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));

    let out = env.run(["codex", "alias", "1"]);

    out.assert_code(1);
    assert!(out.stderr.contains("needs an account"), "{}", out.stderr);
}

#[test]
fn alias_waits_for_the_store_lock() {
    use std::os::fd::AsRawFd;

    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let lock = std::fs::File::create(env.store_dir().join(".lock")).unwrap();
    // SAFETY: flock on a descriptor we own.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let running = env.cmd(["codex", "alias", "1", "work"]).spawn();
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(env.codex_ledger()["accounts"]["1"]["alias"], json!(null));
    drop(lock);
    let out = running.finish(std::time::Duration::from_secs(10));

    out.assert_code(0);
    assert_eq!(env.codex_ledger()["accounts"]["1"]["alias"], "work");
}
