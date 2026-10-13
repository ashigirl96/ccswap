//! Ported from upstream tests/test_migrations.py::TestMacosKeyringToSecurity,
//! ::TestSkips, ::TestAccountNoneFallback, ::TestFailures, ::TestRunner and
//! tests/test_switcher.py::TestUpgradeMigration. Upstream runs with `keyring`
//! unimportable (see upstream_shim.py), so both sides copy legacy items via `security`.

mod support;

use serde_json::{Value, json};
use support::TestEnv;

const LEGACY: &str = "claude-code";
const BACKUP: &str = "claude-swap";
const MIGRATION_ID: &str = "macos_keyring_to_security";

fn seed_accounts(env: &TestEnv, accounts: Value) {
    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [],
        "accounts": accounts,
    }));
}

fn one_account(env: &TestEnv) {
    seed_accounts(
        env,
        json!({"1": {"email": "a@example.com", "organizationUuid": "", "organizationName": ""}}),
    );
}

fn applied(env: &TestEnv) -> Option<Value> {
    let path = env.store_dir().join(".migrations.json");
    path.exists()
        .then(|| env.read_json(&path)["applied"].clone())
}

fn legacy_reads(env: &TestEnv) -> usize {
    env.security()
        .calls()
        .iter()
        .filter(|c| c.argv.first().map(String::as_str) == Some("find-generic-password"))
        .filter(|c| c.argv.windows(2).any(|w| w[0] == "-s" && w[1] == LEGACY))
        .count()
}

#[test]
fn relocates_keyring_creds_to_security() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-a@example.com")
            .as_deref(),
        Some("sekret")
    );
    assert_eq!(
        env.security()
            .item(LEGACY, "account-1-a@example.com")
            .as_deref(),
        Some("sekret"),
        "the security fallback leaves the legacy item for purge"
    );
    assert!(
        out.stderr.contains(
            "claude-swap: migrated 1 macOS credential(s) from the keyring into the Keychain via security\n"
        ),
        "stderr: {}",
        out.stderr
    );
    assert!(applied(&env).unwrap().get(MIGRATION_ID).is_some());
}

#[test]
fn recorded_migration_does_not_run_again() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");
    env.run(["status"]).assert_code(0);
    let reads = legacy_reads(&env);
    assert!(reads > 0);

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(legacy_reads(&env), reads);
    assert!(!out.stderr.contains("migrated"), "stderr: {}", out.stderr);
}

#[test]
fn ids_recorded_by_python_are_honoured() {
    let env = TestEnv::new();
    one_account(&env);
    env.write_file(
        env.store_dir().join(".migrations.json"),
        "{\n  \"version\": 1,\n  \"applied\": {\n    \"macos_keyring_to_security\": \"2026-01-01T00:00:00Z\"\n  }\n}",
    );
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");

    env.run(["status"]).assert_code(0);

    assert_eq!(legacy_reads(&env), 0);
    assert_eq!(env.security().item(BACKUP, "account-1-a@example.com"), None);
}

#[test]
fn recording_keeps_other_ids_in_python_layout() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    seed_accounts(&env, json!({}));
    env.write_file(
        env.store_dir().join(".migrations.json"),
        "{\"version\": 1, \"applied\": {\"windows_keyring_to_files\": \"2026-01-01T00:00:00Z\"}}",
    );

    env.run(["status"]).assert_code(0);

    let path = env.store_dir().join(".migrations.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let state: Value = serde_json::from_str(&text).unwrap();
    let stamp = state["applied"][MIGRATION_ID].as_str().unwrap();
    assert_eq!(
        text,
        format!(
            "{{\n  \"version\": 1,\n  \"applied\": {{\n    \"windows_keyring_to_files\": \"2026-01-01T00:00:00Z\",\n    \"macos_keyring_to_security\": \"{stamp}\"\n  }}\n}}"
        )
    );
    assert_eq!(stamp.len(), "2026-01-01T00:00:00Z".len());
    assert!(stamp.ends_with('Z'));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn noop_when_backup_dir_absent() {
    let env = TestEnv::new();

    env.run(["status"]).assert_code(0);

    assert_eq!(applied(&env), None);
    assert_eq!(legacy_reads(&env), 0);
}

#[test]
fn no_sequence_file_skips_unmarked() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.store_dir()).unwrap();

    env.run(["status"]).assert_code(0);

    assert_eq!(applied(&env), None);
}

#[test]
fn corrupt_sequence_not_marked() {
    let env = TestEnv::new();
    env.write_file(env.store_dir().join("sequence.json"), "{ not json");

    env.run(["status"]);

    assert_eq!(applied(&env), None);
}

#[test]
fn empty_accounts_completes() {
    let env = TestEnv::new();
    seed_accounts(&env, json!({}));

    env.run(["status"]).assert_code(0);

    assert!(applied(&env).unwrap().get(MIGRATION_ID).is_some());
}

#[test]
fn precheck_skips_keyring_when_already_migrated() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(BACKUP, "account-1-a@example.com", "already");
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "old");

    env.run(["status"]).assert_code(0);

    assert_eq!(legacy_reads(&env), 0);
    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-a@example.com")
            .as_deref(),
        Some("already")
    );
    assert!(applied(&env).unwrap().get(MIGRATION_ID).is_some());
}

#[test]
fn a_fallback_enc_does_not_count_as_migrated() {
    let env = TestEnv::new();
    one_account(&env);
    env.write_file(
        env.store_dir()
            .join("credentials/.creds-1-a@example.com.enc"),
        "c3RhbGU=",
    );
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");

    env.run(["status"]).assert_code(0);

    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-a@example.com")
            .as_deref(),
        Some("sekret")
    );
}

#[test]
fn no_keyring_item_is_benign_skip() {
    let env = TestEnv::new();
    one_account(&env);

    env.run(["status"]).assert_code(0);

    assert_eq!(env.security().item(BACKUP, "account-1-a@example.com"), None);
    assert!(applied(&env).unwrap().get(MIGRATION_ID).is_some());
}

#[test]
fn canonical_wins_over_none() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "canonical");
    env.security()
        .set_item(LEGACY, "account-None-a@example.com", "stale-none");

    env.run(["status"]).assert_code(0);

    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-a@example.com")
            .as_deref(),
        Some("canonical")
    );
}

#[test]
fn none_used_as_fallback_when_email_unique() {
    let env = TestEnv::new();
    seed_accounts(&env, json!({"1": {"email": "solo@example.com"}}));
    env.security()
        .set_item(LEGACY, "account-None-solo@example.com", "from-none");

    env.run(["status"]).assert_code(0);

    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-solo@example.com")
            .as_deref(),
        Some("from-none")
    );
}

#[test]
fn none_not_used_for_duplicate_email() {
    let env = TestEnv::new();
    seed_accounts(
        &env,
        json!({
            "1": {"email": "dup@example.com", "organizationUuid": "org-1"},
            "2": {"email": "dup@example.com", "organizationUuid": "org-2"},
        }),
    );
    env.security()
        .set_item(LEGACY, "account-None-dup@example.com", "ambiguous");

    env.run(["status"]).assert_code(0);

    assert_eq!(
        env.security().item(BACKUP, "account-1-dup@example.com"),
        None
    );
    assert_eq!(
        env.security().item(BACKUP, "account-2-dup@example.com"),
        None
    );
    assert_eq!(
        env.security()
            .item(LEGACY, "account-None-dup@example.com")
            .as_deref(),
        Some("ambiguous")
    );
    assert!(applied(&env).unwrap().get(MIGRATION_ID).is_some());
}

#[test]
fn locked_legacy_read_defers_and_retries() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");
    env.security()
        .fail_command_for_service("find-generic-password", LEGACY, 51);

    env.run(["status"]).assert_code(0);
    let reads = legacy_reads(&env);

    assert_eq!(env.security().item(BACKUP, "account-1-a@example.com"), None);
    assert_eq!(applied(&env), None);
    env.run(["status"]).assert_code(0);
    assert!(
        legacy_reads(&env) > reads,
        "an unrecorded migration retries"
    );
}

#[test]
fn partial_failure_migrates_rest_and_stays_unmarked() {
    let env = TestEnv::new();
    seed_accounts(
        &env,
        json!({"1": {"email": "ok@example.com"}, "2": {"email": "bad@example.com"}}),
    );
    env.security()
        .set_item(LEGACY, "account-1-ok@example.com", "good");
    env.security()
        .set_item(LEGACY, "account-2-bad@example.com", "never-read");
    env.security().fail_command_for_item(
        "find-generic-password",
        LEGACY,
        "account-2-bad@example.com",
        51,
    );

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-ok@example.com")
            .as_deref(),
        Some("good")
    );
    assert_eq!(
        env.security().item(BACKUP, "account-2-bad@example.com"),
        None
    );
    assert_eq!(applied(&env), None);
    assert!(
        out.stderr
            .contains("claude-swap: migrated 1 macOS credential(s)"),
        "stderr: {}",
        out.stderr
    );
}

#[test]
fn failed_write_discards_the_new_item_and_stays_unmarked() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");
    env.security().fail_command("add-generic-password", 51);

    env.run(["status"]).assert_code(0);

    assert_eq!(env.security().item(BACKUP, "account-1-a@example.com"), None);
    assert_eq!(
        env.security()
            .item(LEGACY, "account-1-a@example.com")
            .as_deref(),
        Some("sekret")
    );
    assert!(
        !env.store_dir()
            .join("credentials/.creds-1-a@example.com.enc")
            .exists()
    );
    assert_eq!(applied(&env), None);
}

#[test]
fn security_keychain_unusable_defers_and_writes_no_enc() {
    let env = TestEnv::new();
    one_account(&env);
    env.security().fail_with(51);

    env.run(["status"]);

    assert!(
        !env.store_dir()
            .join("credentials/.creds-1-a@example.com.enc")
            .exists()
    );
    assert_eq!(applied(&env), None);
}

#[test]
fn config_commands_run_migrations_too() {
    let env = TestEnv::new();
    one_account(&env);
    env.security()
        .set_item(LEGACY, "account-1-a@example.com", "sekret");

    env.run(["config", "list"]).assert_code(0);

    assert_eq!(
        env.security()
            .item(BACKUP, "account-1-a@example.com")
            .as_deref(),
        Some("sekret")
    );
}

fn pre_v06(env: &TestEnv) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": "user@example.com", "uuid": "user-uuid-1234", "added": "2024-01-01T00:00:00Z"},
            "2": {"email": "other@example.com", "uuid": "other-uuid-5678", "added": "2024-01-02T00:00:00Z"},
        },
    }));
}

#[test]
fn status_after_upgrade_with_org_uuid() {
    let env = TestEnv::new();
    pre_v06(&env);
    env.seed_login("user@example.com", "org-uuid-live", "Live Org");

    let out = env.run(["status", "--json"]);

    out.assert_code(0);
    let active = &out.json()["active"];
    assert_eq!(active["managed"], true);
    assert_eq!(active["number"], 1);
    assert_eq!(active["organizationUuid"], "org-uuid-live");
    assert_eq!(active["organizationName"], "Live Org");
}

#[test]
fn migration_uses_live_config_over_backup_and_appends_fields() {
    let env = TestEnv::new();
    pre_v06(&env);
    env.seed_login("user@example.com", "org-uuid-live", "Live Org");
    env.write_json(
        env.store_dir()
            .join("configs/.claude-config-1-user@example.com.json"),
        &json!({"oauthAccount": {"emailAddress": "user@example.com", "organizationUuid": "org-backup", "organizationName": "Backup Org"}}),
    );
    env.write_json(
        env.store_dir()
            .join("configs/.claude-config-2-other@example.com.json"),
        &json!({"oauthAccount": {"emailAddress": "other@example.com", "organizationUuid": "org-other", "organizationName": "Other Org"}}),
    );

    env.run(["status"]).assert_code(0);

    let data = env.sequence();
    let first = data["accounts"]["1"].as_object().unwrap();
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        [
            "email",
            "uuid",
            "added",
            "organizationUuid",
            "organizationName"
        ]
    );
    assert_eq!(first["organizationUuid"], "org-uuid-live");
    assert_eq!(first["organizationName"], "Live Org");
    assert_eq!(data["accounts"]["2"]["organizationUuid"], "org-other");
    assert_eq!(data["accounts"]["2"]["organizationName"], "Other Org");
    assert_ne!(data["lastUpdated"], "2024-01-01T00:00:00Z");
}

#[test]
fn migration_skips_already_migrated() {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": "user@example.com", "uuid": "user-uuid-1234", "added": "2024-01-01T00:00:00Z",
                  "organizationUuid": "existing-org", "organizationName": "Existing Org"},
            "2": {"email": "other@example.com", "uuid": "other-uuid-5678", "added": "2024-01-02T00:00:00Z"},
        },
    }));
    env.seed_login("user@example.com", "different-org", "Different Org");

    env.run(["status"]).assert_code(0);

    let data = env.sequence();
    assert_eq!(data["accounts"]["1"]["organizationUuid"], "existing-org");
    assert_eq!(data["accounts"]["1"]["organizationName"], "Existing Org");
    assert_eq!(data["accounts"]["2"]["organizationUuid"], "");
    assert_eq!(data["accounts"]["2"]["organizationName"], "");
}

#[test]
fn migration_idempotent() {
    let env = TestEnv::new();
    pre_v06(&env);
    env.seed_login("user@example.com", "org-uuid-live", "Live Org");
    env.run(["status"]).assert_code(0);
    let path = env.store_dir().join("sequence.json");
    let first = std::fs::read_to_string(&path).unwrap();

    env.run(["status"]).assert_code(0);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
}
