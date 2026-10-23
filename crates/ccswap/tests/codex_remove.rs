mod support;

use serde_json::json;
use support::{CodexAuth, TestEnv};

#[test]
fn remove_with_yes_drops_the_row_and_the_managed_home_but_not_the_live_login() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let live = env.seed_codex_login(&CodexAuth::new("b@example.com", "acct-b"));

    let out = env.run(["codex", "remove", "2", "-y"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains("Removed Codex account 2"),
        "{}",
        out.stdout
    );
    let ledger = env.codex_ledger();
    assert_eq!(ledger["sequence"], json!([1]));
    assert!(ledger["accounts"].get("2").is_none(), "{ledger}");
    assert!(!env.codex_managed_home(2).exists());
    assert!(env.codex_managed_home(1).exists());
    assert_eq!(
        std::fs::read_to_string(env.live_codex_auth_path()).unwrap(),
        live
    );
}

#[test]
fn remove_asks_first_and_a_yes_by_email_removes() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));

    let out = env
        .cmd(["codex", "remove", "A@Example.com"])
        .stdin("y\n")
        .run();

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("Are you sure you want to remove Codex account 1 (a@example.com)? [y/N]"),
        "{}",
        out.stdout
    );
    assert!(env.codex_ledger()["accounts"].get("1").is_none());
    assert!(!env.codex_managed_home(1).exists());
}

#[test]
fn remove_keeps_everything_when_the_answer_is_no_or_stdin_ends() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();

    for stdin in ["n\n", ""] {
        let out = env.cmd(["codex", "remove", "1"]).stdin(stdin).run();

        out.assert_code(0);
        assert!(
            out.stdout.contains("Cancelled"),
            "{stdin:?}: {}",
            out.stdout
        );
        assert_eq!(
            std::fs::read(env.codex_ledger_path()).unwrap(),
            ledger_before
        );
        assert!(env.codex_managed_home(1).join("auth.json").exists());
    }
}

#[test]
fn removing_the_active_account_clears_the_ledger_active() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let live = env.seed_codex_login(&CodexAuth::new("b@example.com", "acct-b"));
    let mut ledger = env.codex_ledger();
    ledger["active"] = json!("2");
    env.seed_codex_ledger(ledger);

    let out = env.run(["codex", "remove", "2", "-y"]);

    out.assert_code(0);
    assert!(out.stdout.contains("currently logged in"), "{}", out.stdout);
    assert_eq!(env.codex_ledger()["active"], json!(null));
    assert_eq!(
        std::fs::read_to_string(env.live_codex_auth_path()).unwrap(),
        live
    );
}

#[test]
fn removing_another_account_keeps_the_ledger_active() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    let mut ledger = env.codex_ledger();
    ledger["active"] = json!("1");
    env.seed_codex_ledger(ledger);

    env.run(["codex", "remove", "2", "--yes"]).assert_code(0);

    assert_eq!(env.codex_ledger()["active"], json!("1"));
}

#[test]
fn removing_an_unknown_account_fails_and_changes_nothing() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let ledger_before = std::fs::read(env.codex_ledger_path()).unwrap();

    let out = env.run(["codex", "remove", "5", "-y"]);
    out.assert_code(1);
    assert!(
        out.stderr.contains("Codex account 5 does not exist"),
        "{}",
        out.stderr
    );
    let out = env.run(["codex", "remove", "z@example.com", "-y"]);
    out.assert_code(1);
    assert!(
        out.stderr
            .contains("No Codex account found with identifier: z@example.com"),
        "{}",
        out.stderr
    );
    assert_eq!(
        std::fs::read(env.codex_ledger_path()).unwrap(),
        ledger_before
    );
}

#[test]
fn an_email_in_two_workspaces_must_be_removed_by_number() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-personal"));
    env.seed_codex_account(2, &CodexAuth::new("a@example.com", "acct-work"));

    let out = env.run(["codex", "remove", "a@example.com", "-y"]);

    out.assert_code(1);
    assert!(out.stderr.contains("is ambiguous"), "{}", out.stderr);
    assert_eq!(env.codex_ledger()["sequence"], json!([1, 2]));
}

#[test]
fn remove_waits_for_the_store_lock_before_touching_the_store() {
    use std::os::fd::AsRawFd;

    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    let lock = std::fs::File::create(env.store_dir().join(".lock")).unwrap();
    // SAFETY: flock on a descriptor we own.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let running = env.cmd(["codex", "remove", "1", "-y"]).spawn();
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert!(
        env.codex_managed_home(1).exists(),
        "removed the home while another process held .lock"
    );
    assert_eq!(env.codex_ledger()["sequence"], json!([1]));
    drop(lock);
    let out = running.finish(std::time::Duration::from_secs(10));

    out.assert_code(0);
    assert_eq!(env.codex_ledger()["sequence"], json!([]));
}
