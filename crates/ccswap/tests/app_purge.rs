//! Ported from upstream tests/test_switcher.py::TestPurge.

mod support;

use serde_json::{Value, json};
use support::{KEYCHAIN_ACCOUNT, PROG, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn record(email: &str, org: &str, org_name: &str) -> Value {
    json!({"email": email, "uuid": format!("uuid-{email}"), "organizationUuid": org,
           "organizationName": org_name, "added": "2024-01-01T00:00:00Z"})
}

fn store_creds(env: &TestEnv, num: u32, email: &str) {
    env.security().set_item(
        BACKUP_SERVICE,
        &format!("account-{num}-{email}"),
        &json!({"claudeAiOauth": {"accessToken": format!("sk-{num}"), "refreshToken": format!("rt-{num}")}})
            .to_string(),
    );
    env.write_file(
        config_path(num, email),
        &json!({"oauthAccount": {"emailAddress": email, "accountUuid": format!("uuid-{num}")}})
            .to_string(),
    );
}

fn config_path(num: u32, email: &str) -> String {
    format!(".claude-swap-backup/configs/.claude-config-{num}-{email}.json")
}

fn seed_three(env: &TestEnv) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2, 3],
        "accounts": {
            "1": record("a@example.com", "", ""),
            "2": record("b@example.com", "", ""),
            "3": record("c@example.com", "", ""),
        }
    }));
    for (num, email) in [
        (1, "a@example.com"),
        (2, "b@example.com"),
        (3, "c@example.com"),
    ] {
        store_creds(env, num, email);
    }
}

#[test]
fn help_says_purge_removes_both_providers_data() {
    let env = TestEnv::new();

    let out = env.run(["help"]);

    out.assert_code(0);
    let line = out
        .stdout
        .lines()
        .find(|l| l.starts_with(&format!("  {PROG} purge ")))
        .unwrap_or_else(|| panic!("no purge line in\n{}", out.stdout));
    assert!(
        line.contains("Claude Code") && line.contains("Codex"),
        "{line}"
    );
}

fn purge_banner(env: &TestEnv) -> String {
    format!(
        "This will remove ALL claude-swap data from your system:\n  - Backup directory: {}\n  - All stored account credentials (macOS Keychain and/or files)\n\nNote: This does NOT affect your current Claude Code login.\n\nAre you sure you want to purge all data? [y/N] ",
        env.store_dir().display()
    )
}

#[test]
fn purge_keeps_everything_unless_confirmed() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.cmd(["purge"]).stdin("n\n").run();

    out.assert_code(0);
    assert_eq!(out.stdout, format!("{}Cancelled\n", purge_banner(&env)));
    assert!(env.store_dir().join("sequence.json").exists());
    assert!(
        env.security()
            .item(BACKUP_SERVICE, "account-1-a@example.com")
            .is_some()
    );
}

#[test]
fn purge_removes_every_backup_and_the_store() {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": record("a@example.com", "", ""),
            "2": record("b@example.com", "", ""),
        }
    }));
    store_creds(&env, 1, "a@example.com");
    env.security()
        .set_item(BACKUP_SERVICE, "account-None-a@example.com", "legacy");
    env.write_file(
        env.store_dir()
            .join("credentials/.creds-2-b@example.com.enc"),
        "e30=",
    );

    let out = env.cmd(["purge"]).stdin("y\n").run();

    out.assert_code(0);
    let store = env.store_dir();
    assert_eq!(
        out.stdout,
        format!(
            "{}\nRemoved:\n  - Credential: account-1-a@example.com\n  - Credential: account-None-a@example.com\n  - Credential file: .creds-2-b@example.com.enc\n  - Credential: account-2-b@example.com\n  - Credential: account-None-b@example.com\n  - Directory: {}\n\nPurge complete.\n",
            purge_banner(&env),
            store.display()
        )
    );
    assert!(!store.exists());
    let left: Vec<_> = env
        .security()
        .items()
        .into_iter()
        .filter(|(service, _, _)| service == BACKUP_SERVICE)
        .collect();
    assert_eq!(left, vec![]);
}

#[test]
fn purge_deletes_session_profile_keychain_entries() {
    let env = TestEnv::new();
    seed_three(&env);
    let session = env.store_dir().join("sessions").join("1-a@example.com");
    std::fs::create_dir_all(&session).unwrap();

    let out = env.cmd(["purge"]).stdin("y\n").run();

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("  - All session profiles and their Keychain entries\n")
    );
    assert!(
        out.stdout
            .contains("  - Session profiles: 1-a@example.com\n")
    );
    let deleted_profile_entry = env.security().calls().iter().any(|c| {
        c.argv
            .first()
            .is_some_and(|a| a == "delete-generic-password")
            && c.argv.iter().any(|a| a == KEYCHAIN_ACCOUNT)
            && c.argv
                .iter()
                .any(|a| a.starts_with("Claude Code-credentials-"))
    });
    assert!(deleted_profile_entry);
}

#[test]
fn purge_sweeps_legacy_keyring_items() {
    let env = TestEnv::new();
    seed_three(&env);
    env.security()
        .set_item("claude-code", "account-2-b@example.com", "old");

    let out = env.cmd(["purge"]).stdin("y\n").run();

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("  - Legacy keyring credential: account-2-b@example.com\n")
    );
    assert_eq!(
        env.security()
            .item("claude-code", "account-2-b@example.com"),
        None
    );
}
