//! `remove`/`rm`, `disable`/`enable`, `alias` and `purge`: managing slots that
//! are already registered.
//!
//! Ported from upstream tests/test_switcher.py::TestResolveAccountIdentifier,
//! TestResolveByAlias, TestResolveIdentifierAmbiguity, TestAliasValidation,
//! TestAliasCommand, TestSwitchRemoveGatesAcceptAlias (remove half),
//! TestRemoveAccountPrunesMappings::test_remove_account_prunes_mappings,
//! TestStashAndRetentionStore::test_prev_removed_with_account,
//! TestDisableEnableAccount (flag, message and warning parts), TestPurge, and
//! tests/test_cli.py::TestAliasCommand, TestDisableEnableDispatch,
//! TestSubcommandAliases (rm).

mod support;

use serde_json::{Value, json};
use support::{KEYCHAIN_ACCOUNT, Output, TestEnv, is_upstream, prog};

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

/// Slots 1..=3 (a@, b@, c@example.com), all switchable, slot 1 active.
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

fn set_record_field(env: &TestEnv, num: &str, key: &str, value: Value) {
    let mut data = env.sequence();
    data["accounts"][num][key] = value;
    env.seed_sequence(data);
}

fn err_line(out: &Output) -> &str {
    out.stderr.trim_end()
}

// -- remove ------------------------------------------------------------------

#[test]
fn remove_by_number_deletes_backups_and_record_after_confirming() {
    let env = TestEnv::new();
    seed_three(&env);
    env.security()
        .set_item(BACKUP_SERVICE, "account-2-b@example.com.prev", "old");

    let out = env.cmd(["remove", "2"]).stdin("y\n").run();

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Are you sure you want to permanently remove Account-2 (b@example.com)? [y/N] Removed Account-2 (b@example.com)\n"
    );
    let data = env.sequence();
    assert_eq!(data["sequence"], json!([1, 3]));
    assert!(data["accounts"].get("2").is_none());
    assert_eq!(data["activeAccountNumber"], json!(1));
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com"),
        None
    );
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com.prev"),
        None
    );
    assert!(!env.home().join(config_path(2, "b@example.com")).exists());
    assert!(
        env.security()
            .item(BACKUP_SERVICE, "account-1-a@example.com")
            .is_some()
    );
}

#[test]
fn remove_deletes_file_backups_too() {
    let env = TestEnv::new();
    seed_three(&env);
    let creds_dir = env.store_dir().join("credentials");
    env.write_file(creds_dir.join(".creds-2-b@example.com.enc"), "e30=");
    env.write_file(creds_dir.join(".creds-2-b@example.com.enc.prev"), "e30=");

    env.cmd(["rm", "2"]).stdin("y\n").run().assert_code(0);

    assert!(!creds_dir.join(".creds-2-b@example.com.enc").exists());
    assert!(!creds_dir.join(".creds-2-b@example.com.enc.prev").exists());
}

#[test]
fn remove_is_cancelled_by_anything_but_y() {
    let env = TestEnv::new();
    seed_three(&env);
    let before = env.sequence();

    for answer in ["n\n", "yes\n", "\n"] {
        let out = env.cmd(["remove", "2"]).stdin(answer).run();
        out.assert_code(0);
        assert!(out.stdout.ends_with("[y/N] Cancelled\n"), "{}", out.stdout);
    }
    assert_eq!(env.sequence(), before);
    assert!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com")
            .is_some()
    );

    env.cmd(["remove", "2"]).stdin("Y\n").run().assert_code(0);
    assert!(env.sequence()["accounts"].get("2").is_none());
}

#[test]
fn removing_the_active_account_warns_and_leaves_active_number() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env
        .cmd(["--remove-account", "a@example.com"])
        .stdin("y\n")
        .run();

    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Warning: Account-1 (a@example.com) is currently active\n"),
        "{}",
        out.stdout
    );
    let data = env.sequence();
    assert_eq!(data["sequence"], json!([2, 3]));
    assert_eq!(data["activeAccountNumber"], json!(1));
}

#[test]
fn remove_by_alias() {
    let env = TestEnv::new();
    seed_three(&env);
    set_record_field(&env, "2", "alias", json!("dev"));

    env.cmd(["remove", "DEV"]).stdin("y\n").run().assert_code(0);

    let data = env.sequence();
    assert!(data["accounts"].get("2").is_none());
    assert!(data["accounts"].get("1").is_some());
}

#[test]
fn remove_reports_unresolvable_identifiers() {
    let env = TestEnv::new();
    let out = env.run(["remove", "1"]);
    out.assert_code(1);
    assert_eq!(err_line(&out), "Error: No accounts are managed yet");

    seed_three(&env);
    let cases = [
        (
            "not an email or alias!",
            "Error: Invalid account identifier: not an email or alias!",
        ),
        (
            "nobody@example.com",
            "Error: No account found with identifier: nobody@example.com",
        ),
        ("99", "Error: Account-99 does not exist"),
        ("dev", "Error: Invalid account identifier: dev"),
    ];
    for (identifier, expected) in cases {
        let out = env.run(["remove", identifier]);
        out.assert_code(1);
        assert_eq!(err_line(&out), expected, "{identifier}");
        assert_eq!(out.stdout, "");
    }
}

#[test]
fn remove_asks_which_slot_when_an_email_is_in_two_orgs() {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": record("u@example.com", "", ""),
            "2": record("u@example.com", "org-2", "Acme"),
        }
    }));

    let out = env.cmd(["remove", "u@example.com"]).stdin("7\n").run();
    out.assert_code(0);
    assert!(
        out.stdout
            .ends_with("Enter account number to remove: Cancelled\n")
    );

    let out = env.cmd(["remove", "u@example.com"]).stdin("2\ny\n").run();

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Multiple accounts found for 'u@example.com':\n  1: u@example.com [personal]\n  2: u@example.com [Acme]\nEnter account number to remove: Are you sure you want to permanently remove Account-2 (u@example.com)? [y/N] Removed Account-2 (u@example.com)\n"
    );
    assert_eq!(env.sequence()["sequence"], json!([1]));
}

#[test]
fn remove_prunes_the_accounts_directory_mappings() {
    let env = TestEnv::new();
    seed_three(&env);
    env.write_json(
        env.store_dir().join("mappings.json"),
        &json!({"schemaVersion": 1, "mappings": {
            "/w/b": {"email": "b@example.com", "organizationUuid": "", "added": "2024-01-01T00:00:00Z"},
            "/w/a": {"email": "a@example.com", "organizationUuid": "", "added": "2024-01-01T00:00:00Z"}
        }}),
    );

    let out = env.cmd(["remove", "2"]).stdin("y\n").run();

    out.assert_code(0);
    assert!(
        out.stdout.ends_with(
            "Removed Account-2 (b@example.com)\nRemoved 1 directory mapping(s) for this account\n"
        ),
        "{}",
        out.stdout
    );
    let mappings = env.read_json(env.store_dir().join("mappings.json"));
    assert_eq!(
        mappings["mappings"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["/w/a"]
    );
}

#[test]
fn remove_without_an_identifier_is_a_usage_error() {
    let env = TestEnv::new();
    let out = env.run(["remove"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!(
            "usage: {p} <command> [args] [options]\n{p}: error: argument --remove-account: expected one argument\n",
            p = prog()
        )
    );
}

// -- disable / enable ----------------------------------------------------------

#[test]
fn disable_sets_the_flag_and_keeps_the_slot_explicitly_reachable() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.run(["disable", "2"]);

    out.assert_code(0);
    assert_eq!(out.stdout, "Disabled Account-2 (b@example.com).\n");
    let data = env.sequence();
    assert_eq!(data["accounts"]["2"]["disabled"], json!(true));
    assert_eq!(
        data["accounts"]["2"]
            .as_object()
            .unwrap()
            .keys()
            .next_back()
            .unwrap(),
        "disabled"
    );
    assert_eq!(data["sequence"], json!([1, 2, 3]));
    assert!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com")
            .is_some()
    );
    assert!(env.home().join(config_path(2, "b@example.com")).exists());
}

#[test]
fn enable_removes_the_flag() {
    let env = TestEnv::new();
    seed_three(&env);
    env.run(["disable", "2"]).assert_code(0);

    let out = env.run(["enable", "2"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Enabled Account-2 (b@example.com).\n  It is back in the rotation.\n"
    );
    let data = env.sequence();
    assert!(data["accounts"]["2"].get("disabled").is_none());
    assert_eq!(data["sequence"], json!([1, 2, 3]));
}

#[test]
fn disable_and_enable_accept_email_alias_and_legacy_flags() {
    let env = TestEnv::new();
    seed_three(&env);
    set_record_field(&env, "3", "alias", json!("dev"));

    env.run(["disable", "b@example.com"]).assert_code(0);
    env.run(["--disable-account", "DEV"]).assert_code(0);
    let data = env.sequence();
    assert_eq!(data["accounts"]["2"]["disabled"], json!(true));
    assert_eq!(data["accounts"]["3"]["disabled"], json!(true));

    env.run(["--enable-account", "dev"]).assert_code(0);
    env.run(["enable", "b@example.com"]).assert_code(0);
    let data = env.sequence();
    assert!(data["accounts"]["2"].get("disabled").is_none());
    assert!(data["accounts"]["3"].get("disabled").is_none());
}

#[test]
fn repeating_disable_or_enable_is_a_noop() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.run(["enable", "1"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Account-1 (a@example.com) is already enabled.\n"
    );

    env.run(["disable", "2"]).assert_code(0);
    let before = env.sequence();
    let out = env.run(["disable", "2"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Account-2 (b@example.com) is already disabled.\n"
    );
    assert_eq!(env.sequence(), before);
}

#[test]
fn disabling_the_active_account_says_it_stays_live() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.run(["disable", "1"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Disabled Account-1 (a@example.com).\n  It is the active account — it stays live until you switch away; it just won't be an automatic switch target.\n"
    );
}

#[test]
fn disabling_the_last_switchable_slot_warns() {
    let env = TestEnv::new();
    seed_three(&env);
    env.security()
        .set_item(BACKUP_SERVICE, "account-3-c@example.com", "");

    env.run(["disable", "2"]).assert_code(0);
    let out = env.run(["disable", "1"]);

    out.assert_code(0);
    assert!(
        out.stdout.ends_with(
            "  No accounts remain in rotation — auto-switch and bare switch have nothing to pick. Re-enable one with cswap enable <num|email>.\n"
        ),
        "{}",
        out.stdout
    );
}

#[test]
fn disable_reports_unresolvable_identifiers() {
    let env = TestEnv::new();
    let out = env.run(["disable", "1"]);
    out.assert_code(1);
    assert_eq!(err_line(&out), "Error: No accounts are managed yet");

    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": record("u@example.com", "", ""),
            "2": record("u@example.com", "org-2", "Acme"),
        }
    }));
    let cases = [
        ("99", "Error: Account-99 does not exist"),
        ("01", "Error: Account-01 does not exist"),
        (
            "x@example.com",
            "Error: No account found with identifier: x@example.com",
        ),
        (
            "u@example.com",
            "Error: Email 'u@example.com' is ambiguous — matches accounts: 1 [personal], 2 [Acme]. Use account number instead (e.g., cswap --switch-to 1).",
        ),
    ];
    for (identifier, expected) in cases {
        let out = env.run(["enable", identifier]);
        out.assert_code(1);
        assert_eq!(err_line(&out), expected, "{identifier}");
    }
}

#[test]
fn disable_without_a_target_is_a_usage_error() {
    let env = TestEnv::new();
    env.run(["disable"]).assert_code(2);
}

// -- alias ---------------------------------------------------------------------

#[test]
fn alias_sets_by_number_or_email_and_lowercases() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.run(["alias", "2", "DEV"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Set alias 'dev' for Account 2\n");
    assert_eq!(env.sequence()["accounts"]["2"]["alias"], json!("dev"));

    let out = env.run(["alias", "c@example.com", "ops"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Set alias 'ops' for Account 3\n");
    assert_eq!(env.sequence()["accounts"]["3"]["alias"], json!("ops"));
}

#[test]
fn alias_renames_through_the_existing_alias() {
    let env = TestEnv::new();
    seed_three(&env);
    env.run(["alias", "1", "dev"]).assert_code(0);

    let out = env.run(["alias", "dev", "prod"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Set alias 'prod' for Account 1\n");

    let out = env.run(["alias", "dev", "again"]);
    out.assert_code(1);
    assert_eq!(
        err_line(&out),
        "Error: No account found with identifier: dev"
    );
}

#[test]
fn alias_rejects_invalid_and_duplicate_names() {
    let env = TestEnv::new();
    seed_three(&env);
    env.run(["alias", "1", "dev"]).assert_code(0);
    let before = env.sequence();

    let cases: [(&[&str], &str); 5] = [
        (
            &["2", "123"],
            "Error: alias '123' cannot be purely numeric (reserved for slot numbers)",
        ),
        (
            &["2", "dev@work"],
            "Error: alias 'dev@work' may only contain letters, digits, '-', '_', and '.'",
        ),
        (
            &["2", "dev work"],
            "Error: alias 'dev work' may only contain letters, digits, '-', '_', and '.'",
        ),
        (
            &["2", "--", "-dev"],
            "Error: alias '-dev' cannot start with '-' (would be read as a command flag)",
        ),
        (
            &["2", "DEV"],
            "Error: Alias 'dev' is already used by account 1",
        ),
    ];
    for (args, expected) in cases {
        let out = env.run(std::iter::once("alias").chain(args.iter().copied()));
        out.assert_code(1);
        assert_eq!(err_line(&out), expected, "{args:?}");
    }
    assert_eq!(env.sequence(), before);
}

#[test]
fn alias_reports_unknown_accounts() {
    let env = TestEnv::new();
    let out = env.run(["alias", "2", "dev"]);
    out.assert_code(1);
    assert_eq!(err_line(&out), "Error: Account-2 does not exist");

    seed_three(&env);
    let out = env.run(["alias", "999", "dev"]);
    out.assert_code(1);
    assert_eq!(err_line(&out), "Error: Account-999 does not exist");
}

#[test]
fn alias_unset_is_idempotent() {
    let env = TestEnv::new();
    seed_three(&env);
    set_record_field(&env, "2", "alias", json!("dev"));

    for _ in 0..2 {
        let out = env.run(["alias", "2", "--unset"]);
        out.assert_code(0);
        assert_eq!(out.stdout, "Removed alias for Account 2\n");
        assert!(env.sequence()["accounts"]["2"].get("alias").is_none());
    }
}

#[test]
fn alias_lists_by_slot_number() {
    let env = TestEnv::new();
    let out = env.run(["alias"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "No aliases set\n");

    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [2, 10, 3],
        "accounts": {
            "10": record("ten@example.com", "", ""),
            "3": record("three@example.com", "", ""),
            "2": record("two@example.com", "", ""),
        }
    }));
    set_record_field(&env, "10", "alias", json!("ten"));
    set_record_field(&env, "2", "alias", json!("two"));

    let out = env.run(["alias"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Aliases:\n  2: two (two@example.com)\n  10: ten (ten@example.com)\n"
    );
}

#[test]
fn alias_argument_errors_exit_2() {
    let env = TestEnv::new();
    seed_three(&env);
    let p = prog();
    let cases: [(&[&str], &str); 5] = [
        (
            &["2"],
            "NAME is required (or pass --unset to remove the alias)",
        ),
        (&["--unset"], "NUM|EMAIL is required with --unset"),
        (
            &["2", "dev", "--unset"],
            "--unset does not take a NAME argument",
        ),
        (&["2", "dev", "extra"], "unrecognized arguments: extra"),
        (
            &["--unset=x", "2"],
            "argument --unset: ignored explicit argument 'x'",
        ),
    ];
    for (args, msg) in cases {
        let out = env.run(std::iter::once("alias").chain(args.iter().copied()));
        out.assert_code(2);
        assert_eq!(
            out.stderr,
            format!(
                "usage: {p} alias [-h] [--unset] [--debug] [NUM|EMAIL] [NAME]\n{p} alias: error: {msg}\n"
            ),
            "{args:?}"
        );
    }
}

#[test]
fn alias_help_describes_the_command() {
    let env = TestEnv::new();
    let out = env.run(["alias", "-h"]);
    out.assert_code(0);
    let p = prog();
    assert!(
        out.stdout.starts_with(&format!(
            "usage: {p} alias [-h] [--unset] [--debug] [NUM|EMAIL] [NAME]\n\nSet, remove, or list a short display alias"
        )),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains(&format!("  {p} alias 2 --unset\n")));
}

// -- purge ---------------------------------------------------------------------

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
    if is_upstream() {
        // Upstream reaches these through the `keyring` library, which the
        // differential shim disables.
        return;
    }
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
