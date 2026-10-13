//! `add-token` (`--add-token`): register a setup-token or managed API key
//! without a browser login.
//!
//! Ported from upstream tests/test_switcher.py::TestAddAccountFromToken,
//! test_api_key_accounts.py::TestKindDetection / TestAddTokenApiKey /
//! TestCrossKindCollision and test_cli.py (add-token dispatch and flag checks).

mod support;

use serde_json::{Value, json};
use support::{TestEnv, is_upstream};

const BACKUP_SERVICE: &str = "claude-swap";
const API_KEY: &str = "sk-ant-api03-a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5a1b2c3d4e5";
const OTHER_KEY: &str = "sk-ant-api03-z9y8x7w6v5z9y8x7w6v5z9y8x7w6v5z9y8x7w6v5";

fn sequence(env: &TestEnv) -> Value {
    env.read_json(env.store_dir().join("sequence.json"))
}

fn backup(env: &TestEnv, num: u32, email: &str) -> Option<String> {
    env.security()
        .item(BACKUP_SERVICE, &format!("account-{num}-{email}"))
}

fn setup_token_creds(token: &str) -> String {
    format!(r#"{{"claudeAiOauth": {{"accessToken": "{token}", "scopes": ["user:inference"]}}}}"#)
}

fn config_text(env: &TestEnv, num: u32, email: &str) -> String {
    std::fs::read_to_string(
        env.store_dir()
            .join(format!("configs/.claude-config-{num}-{email}.json")),
    )
    .unwrap()
}

fn seed_three(env: &TestEnv) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 3],
        "accounts": {
            "1": {"email": "one@example.com", "uuid": "u1", "organizationUuid": "",
                  "organizationName": "", "added": "2024-01-01T00:00:00Z"},
            "3": {"email": "three@example.com", "uuid": "u3", "organizationUuid": "org-3",
                  "organizationName": "Three Inc", "added": "2024-01-01T00:00:00Z"}
        }
    }));
}

fn seed_dead_token_row(env: &TestEnv, num: &str, email: &str) {
    env.write_json(
        env.store_dir().join("cache/usage.json"),
        &json!({"schemaVersion": 2, "accounts": {
            num: {"email": email, "organizationUuid": "", "authDeadStrikes": 3,
                  "lastError": "invalid_grant", "consecutiveFailures": 2}
        }}),
    );
}

fn assert_quarantine_lifted(env: &TestEnv, num: &str) {
    let usage = env.read_json(env.store_dir().join("cache/usage.json"));
    let row = &usage["accounts"][num];
    assert_eq!(row["authDeadStrikes"], 0, "{usage}");
    assert_eq!(row["lastError"], Value::Null, "{usage}");
    assert_eq!(row["consecutiveFailures"], 0, "{usage}");
}

#[test]
fn a_setup_token_with_an_email_is_stored_as_an_oauth_account() {
    let env = TestEnv::new();

    let out = env.run([
        "add-token",
        "sk-ant-oat01-abc",
        "--email",
        "user@example.com",
    ]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Added Account 1: user@example.com [personal] (from token)\n"
    );

    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([1]));
    assert_eq!(
        seq["activeAccountNumber"],
        Value::Null,
        "add-token never activates"
    );
    let record = seq["accounts"]["1"].as_object().unwrap();
    let keys: Vec<&str> = record.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "email",
            "uuid",
            "organizationUuid",
            "organizationName",
            "added"
        ],
        "no kind key for a setup-token"
    );
    assert_eq!(record["email"], "user@example.com");
    assert_eq!(record["uuid"], "");
    assert_eq!(record["organizationUuid"], "");
    assert_eq!(record["organizationName"], "");

    assert_eq!(
        backup(&env, 1, "user@example.com").as_deref(),
        Some(setup_token_creds("sk-ant-oat01-abc").as_str())
    );
    assert_eq!(
        config_text(&env, 1, "user@example.com"),
        r#"{"oauthAccount": {"emailAddress": "user@example.com", "accountUuid": "", "organizationUuid": null, "organizationName": null}}"#
    );
    let api_calls: Vec<String> = env
        .http()
        .requests()
        .iter()
        .map(|r| r.path().to_string())
        .filter(|p| p != "/update-check")
        .collect();
    assert!(api_calls.is_empty(), "no Anthropic calls: {api_calls:?}");
}

#[test]
fn an_api_key_is_stored_raw_with_the_api_key_kind_and_default_email() {
    let env = TestEnv::new();

    let out = env.run(["add-token", API_KEY]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Added Account 1: api-key-1@token.local [personal] (from API key)\n"
    );
    let record = &sequence(&env)["accounts"]["1"];
    assert_eq!(record["email"], "api-key-1@token.local");
    assert_eq!(record["kind"], "api_key");
    let keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.last(), Some(&"kind"));
    assert_eq!(
        backup(&env, 1, "api-key-1@token.local").as_deref(),
        Some(API_KEY)
    );
}

#[test]
fn a_setup_token_without_email_gets_the_setup_token_label() {
    let env = TestEnv::new();

    let out = env.run(["add-token", "sk-ant-oat01-abc"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Added Account 1: setup-token-1@token.local [personal] (from token)\n"
    );
    let record = &sequence(&env)["accounts"]["1"];
    assert_eq!(record["email"], "setup-token-1@token.local");
    assert!(record.get("kind").is_none());
    assert!(
        config_text(&env, 1, "setup-token-1@token.local")
            .contains(r#""emailAddress": "setup-token-1@token.local""#)
    );
}

#[test]
fn something_that_only_contains_an_api_key_is_a_setup_token() {
    let env = TestEnv::new();

    env.run(["add-token", r#"{"x": "sk-ant-api03-inside-json"}"#])
        .assert_code(0);
    assert_eq!(
        sequence(&env)["accounts"]["1"]["email"],
        "setup-token-1@token.local"
    );
}

#[test]
fn the_default_email_follows_an_explicit_slot() {
    let env = TestEnv::new();

    env.run(["add-token", "tok-a", "--slot", "4"])
        .assert_code(0);
    env.run(["add-token", "tok-b", "--slot", "8"])
        .assert_code(0);

    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([4, 8]));
    assert_eq!(seq["accounts"]["4"]["email"], "setup-token-4@token.local");
    assert_eq!(seq["accounts"]["8"]["email"], "setup-token-8@token.local");
    assert!(seq["accounts"].get("1").is_none());
}

#[test]
fn repeated_default_registrations_take_new_slots() {
    let env = TestEnv::new();
    seed_three(&env);

    let out = env.run(["add-token", "tok"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Added Account 4: setup-token-4@token.local [personal] (from token)\n"
    );
    env.run(["add-token", "tok"]).assert_code(0);
    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([1, 3, 4, 5]));
    assert_eq!(seq["activeAccountNumber"], 1);
}

#[test]
fn an_explicit_email_wins_over_the_default() {
    let env = TestEnv::new();

    env.run([
        "add-token",
        "tok",
        "--email",
        "me@example.com",
        "--slot",
        "2",
    ])
    .assert_code(0);
    assert_eq!(sequence(&env)["accounts"]["2"]["email"], "me@example.com");
}

#[test]
fn the_sequence_stays_sorted_with_explicit_slots() {
    let env = TestEnv::new();

    env.run([
        "add-token",
        "tok",
        "--email",
        "a@example.com",
        "--slot",
        "5",
    ])
    .assert_code(0);
    env.run([
        "add-token",
        "tok",
        "--email",
        "b@example.com",
        "--slot",
        "2",
    ])
    .assert_code(0);
    assert_eq!(sequence(&env)["sequence"], json!([2, 5]));
}

#[test]
fn dash_reads_the_token_from_one_stdin_line() {
    let env = TestEnv::new();

    let out = env
        .cmd(["add-token", "-", "--email", "user@example.com"])
        .stdin("  stdin-token  \nsecond line\n")
        .run();
    out.assert_code(0);
    assert_eq!(
        backup(&env, 1, "user@example.com").as_deref(),
        Some(setup_token_creds("stdin-token").as_str())
    );
}

#[test]
fn readding_the_same_email_refreshes_the_token_in_place() {
    let env = TestEnv::new();
    seed_three(&env);
    env.run(["add-token", "token-v1", "--email", "user@example.com"])
        .assert_code(0);
    let added = sequence(&env)["accounts"]["4"]["added"].clone();

    let out = env.run(["add-token", "token-v2", "--email", "user@example.com"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Updated token for Account 4 (user@example.com [personal]).\n"
    );
    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([1, 3, 4]));
    assert_eq!(seq["activeAccountNumber"], 1, "refresh never activates");
    assert_eq!(seq["accounts"]["4"]["added"], added);
    assert_eq!(
        backup(&env, 4, "user@example.com").as_deref(),
        Some(setup_token_creds("token-v2").as_str())
    );
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-4-user@example.com.prev")
            .as_deref(),
        Some(setup_token_creds("token-v1").as_str()),
        "the previous generation is retained"
    );
}

#[test]
fn readding_the_same_api_key_account_refreshes_it_in_place() {
    let env = TestEnv::new();
    env.run(["add-token", API_KEY, "--email", "me@example.com"])
        .assert_code(0);

    let out = env.run(["add-token", OTHER_KEY, "--email", "me@example.com"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Updated API key for Account 1 (me@example.com [personal]).\n"
    );
    assert_eq!(sequence(&env)["accounts"].as_object().unwrap().len(), 1);
    assert_eq!(
        backup(&env, 1, "me@example.com").as_deref(),
        Some(OTHER_KEY)
    );
}

#[test]
fn a_refresh_in_place_lifts_the_dead_token_quarantine() {
    let env = TestEnv::new();
    env.run(["add-token", "token-v1", "--email", "user@example.com"])
        .assert_code(0);
    seed_dead_token_row(&env, "1", "user@example.com");

    env.run(["add-token", "token-v2", "--email", "user@example.com"])
        .assert_code(0);
    assert_quarantine_lifted(&env, "1");
}

#[test]
fn a_new_registration_lifts_a_stale_quarantine_on_its_slot() {
    let env = TestEnv::new();
    seed_dead_token_row(&env, "5", "user@example.com");

    env.run([
        "add-token",
        "tok",
        "--email",
        "user@example.com",
        "--slot",
        "5",
    ])
    .assert_code(0);
    assert_quarantine_lifted(&env, "5");
}

#[test]
fn an_invalid_email_is_refused() {
    let env = TestEnv::new();

    let out = env.run(["add-token", "tok", "--email", "not-an-email"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Invalid email format: not-an-email\n");
    assert!(!env.store_dir().join("sequence.json").exists());
}

#[test]
fn a_blank_token_is_refused() {
    let env = TestEnv::new();

    let out = env.run(["add-token", "   ", "--email", "user@example.com"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Token cannot be empty\n");

    let out = env
        .cmd(["add-token", "-", "--email", "user@example.com"])
        .stdin("")
        .run();
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Token cannot be empty\n");
}

#[test]
fn slot_below_one_is_refused() {
    let env = TestEnv::new();

    let out = env.run([
        "add-token",
        "tok",
        "--email",
        "user@example.com",
        "--slot",
        "0",
    ]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Slot number must be >= 1\n");
}

#[test]
fn an_api_key_cannot_take_over_an_oauth_email() {
    let env = TestEnv::new();
    env.run([
        "add-token",
        "sk-ant-oat01-abc",
        "--email",
        "dup@example.com",
    ])
    .assert_code(0);

    let out = env.run(["add-token", API_KEY, "--email", "dup@example.com"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: 'dup@example.com' already exists as an OAuth account (slot 1); cannot add it as an API-key account. Pass a distinct --email.\n"
    );
}

#[test]
fn a_setup_token_cannot_take_over_an_api_key_email() {
    let env = TestEnv::new();
    env.run(["add-token", API_KEY, "--email", "dup@example.com"])
        .assert_code(0);

    let out = env.run([
        "add-token",
        "sk-ant-oat01-abc",
        "--email",
        "dup@example.com",
    ]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: 'dup@example.com' already exists as an API-key account (slot 1); cannot add it as an OAuth account. Pass a distinct --email.\n"
    );
    assert_eq!(backup(&env, 1, "dup@example.com").as_deref(), Some(API_KEY));
}

#[test]
fn an_occupied_slot_asks_before_overwriting() {
    let env = TestEnv::new();
    seed_three(&env);
    let before = std::fs::read_to_string(env.store_dir().join("sequence.json")).unwrap();

    let out = env
        .cmd([
            "add-token",
            "tok",
            "--email",
            "new@example.com",
            "--slot",
            "3",
        ])
        .stdin("no\n")
        .run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Slot 3 already occupied\nthree@example.com [Three Inc]\nOverwrite slot 3? [y/N] Cancelled\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.store_dir().join("sequence.json")).unwrap(),
        before
    );
}

#[test]
fn an_occupied_slot_is_replaced_after_yes() {
    let env = TestEnv::new();
    seed_three(&env);
    let store = env.store_dir();
    env.write_file(
        store.join("configs/.claude-config-3-three@example.com.json"),
        "{}",
    );
    env.security()
        .set_item(BACKUP_SERVICE, "account-3-three@example.com", "OLD");
    env.write_json(
        store.join("mappings.json"),
        &json!({"schemaVersion": 1, "mappings": {
            "/w/three": {"email": "three@example.com", "organizationUuid": "org-3", "added": "2024-01-01T00:00:00Z"}
        }}),
    );

    let out = env
        .cmd([
            "add-token",
            "tok",
            "--email",
            "new@example.com",
            "--slot",
            "3",
        ])
        .stdin("YES\n")
        .run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Slot 3 already occupied\nthree@example.com [Three Inc]\nOverwrite slot 3? [y/N] \
         Removed 1 directory mapping(s) for this account\n\
         Added Account 3: new@example.com [personal] (from token)\n"
    );
    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([1, 3]));
    assert_eq!(seq["accounts"]["3"]["email"], "new@example.com");
    assert_eq!(seq["activeAccountNumber"], 1);
    assert!(
        !store
            .join("configs/.claude-config-3-three@example.com.json")
            .exists()
    );
    assert_eq!(backup(&env, 3, "three@example.com"), None);
}

#[test]
fn slot_flag_moves_the_same_email_to_the_new_slot() {
    let env = TestEnv::new();
    env.run(["add-token", "tok-1", "--email", "user@example.com"])
        .assert_code(0);

    let out = env.run([
        "add-token",
        "tok-2",
        "--email",
        "user@example.com",
        "--slot",
        "4",
    ]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Moved from slot 1 → 4\nAdded Account 4: user@example.com [personal] (from token)\n"
    );
    let seq = sequence(&env);
    assert_eq!(seq["sequence"], json!([4]));
    assert!(seq["accounts"].get("1").is_none());
    assert_eq!(backup(&env, 1, "user@example.com"), None);
    assert_eq!(
        backup(&env, 4, "user@example.com").as_deref(),
        Some(setup_token_creds("tok-2").as_str())
    );
}

#[test]
fn the_flag_spelling_works_with_email_and_slot() {
    let env = TestEnv::new();

    env.run([
        "--add-token",
        "tok",
        "--email",
        "u@example.com",
        "--slot",
        "3",
    ])
    .assert_code(0);
    assert_eq!(sequence(&env)["accounts"]["3"]["email"], "u@example.com");
}

#[test]
fn email_without_add_token_is_a_usage_error() {
    let env = TestEnv::new();

    let out = env.run(["add", "--email", "u@example.com"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .ends_with("error: --email can only be used with 'add-token'\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn alias_and_json_are_not_accepted_with_add_token() {
    let env = TestEnv::new();

    let out = env.run(["add-token", "tok", "--alias", "dev"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .ends_with("error: --alias can only be used with 'add'\n"),
        "{}",
        out.stderr
    );

    let out = env.run(["add-token", "tok", "--json"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .ends_with("error: --json can only be used with 'list', 'status', or 'switch'\n"),
        "{}",
        out.stderr
    );
}

#[test]
fn add_token_inside_a_session_profile_is_refused() {
    let env = TestEnv::new();
    let session = env.store_dir().join("sessions/1-a");
    std::fs::create_dir_all(&session).unwrap();

    let out = env
        .cmd(["add-token", "tok"])
        .env("CLAUDE_CONFIG_DIR", session.to_str().unwrap())
        .run();
    out.assert_code(1);
    assert!(
        out.stderr
            .starts_with("Error: This shell is inside a cswap run session profile"),
        "{}",
        out.stderr
    );
    assert!(!env.store_dir().join("sequence.json").exists());
}

#[test]
fn ccswap_refreshes_a_token_account_cswap_registered() {
    if !is_upstream() {
        eprintln!("skipped: set CCSWAP_TEST_UPSTREAM to run the compatibility round trip");
        return;
    }
    let env = TestEnv::new();
    env.cmd(["add-token", API_KEY])
        .using(support::Impl::Upstream)
        .run()
        .assert_code(0);

    let out = env
        .cmd(["add-token", OTHER_KEY, "--email", "api-key-1@token.local"])
        .using(support::Impl::Ccswap)
        .run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Updated API key for Account 1 (api-key-1@token.local [personal]).\n"
    );
    assert_eq!(
        backup(&env, 1, "api-key-1@token.local").as_deref(),
        Some(OTHER_KEY)
    );
}
