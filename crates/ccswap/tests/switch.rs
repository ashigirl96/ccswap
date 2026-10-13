//! `switch` / `switch <num|email|alias>`: the normal switch transaction.
//!
//! Ported from upstream tests/test_switcher.py::TestPerformSwitchPostDisplay,
//! TestSwitchSkipsBrokenSlots, TestSharedOAuthCredentialPreservation,
//! TestClaudeCodeLockCooperation, TestResolveIdentifierAmbiguity,
//! TestSwitchToSelfSlotAndForce (self no-op), test_session_shell_config_dir_refuses_switch,
//! test_json_output.py::TestSwitchJson (normal-path cases) and
//! test_api_key_accounts.py::test_switch_back_to_oauth_clears_key. The
//! normal-path rollback has no upstream test; the rollback cases here are new.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, MANAGED_KEY_SERVICE, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";
const KEYCHAIN_FOLLOWUP: &str = "Restart Claude Code to apply immediately — otherwise the session can take up to ~30 seconds to pick up the new account.";
const FILE_FOLLOWUP: &str = "New account is active on your next message — no restart needed.";

fn oauth(access: &str, refresh: &str) -> Value {
    json!({"claudeAiOauth": {
        "accessToken": access,
        "refreshToken": refresh,
        "expiresAt": 9_999_999_999_000_i64,
    }})
}

fn creds(tag: &str) -> String {
    oauth(&format!("at-{tag}"), &format!("rt-{tag}")).to_string()
}

fn email(num: u32) -> String {
    ["", "a@example.com", "b@example.com", "c@example.com"][num as usize].to_string()
}

fn backup_account(num: u32) -> String {
    format!("account-{num}-{}", email(num))
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}")})
}

fn record(num: u32) -> Value {
    json!({"email": email(num), "uuid": format!("uuid-{num}"), "organizationUuid": "",
           "organizationName": "", "added": "2024-01-01T00:00:00Z"})
}

/// Slots `nums` each with a Keychain backup `creds("<num>")` and a config
/// backup; the live login is slot `active` holding the same bytes.
fn seed(env: &TestEnv, nums: &[u32], active: u32) {
    let accounts: serde_json::Map<String, Value> =
        nums.iter().map(|n| (n.to_string(), record(*n))).collect();
    env.seed_sequence(json!({
        "activeAccountNumber": active,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": nums,
        "accounts": accounts,
    }));
    for n in nums {
        env.security()
            .set_item(BACKUP_SERVICE, &backup_account(*n), &creds(&n.to_string()));
        write_config_backup(
            env,
            *n,
            &json!({"oauthAccount": oauth_account(*n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({
            "oauthAccount": oauth_account(active),
            "projects": {"/work": {"allowedTools": []}},
            "userID": "caf\u{e9}",
        }),
    );
    set_live(env, &creds(&active.to_string()));
}

fn write_config_backup(env: &TestEnv, num: u32, text: &str) -> PathBuf {
    let path = env
        .store_dir()
        .join(format!("configs/.claude-config-{num}-{}.json", email(num)));
    env.write_file(&path, text);
    path
}

fn set_live(env: &TestEnv, value: &str) {
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, value);
}

fn live(env: &TestEnv) -> Option<String> {
    env.security().item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)
}

fn backup(env: &TestEnv, num: u32) -> Option<String> {
    env.security().item(BACKUP_SERVICE, &backup_account(num))
}

fn prev(env: &TestEnv, num: u32) -> Option<String> {
    env.security()
        .item(BACKUP_SERVICE, &format!("{}.prev", backup_account(num)))
}

fn sequence(env: &TestEnv) -> Value {
    env.read_json(env.store_dir().join("sequence.json"))
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn backup_writes(env: &TestEnv, num: u32) -> usize {
    let account = format!("\"{}\"", backup_account(num));
    env.security()
        .calls()
        .iter()
        .filter(|c| {
            c.stdin
                .as_deref()
                .is_some_and(|s| s.contains("add-generic-password") && s.contains(&account))
        })
        .count()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// test_switch_persists_rotated_refresh_token_to_backup
#[test]
fn switch_backs_up_the_rotated_outgoing_login_then_activates_the_next_slot() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-1-new", "rt-1").to_string();
    set_live(&env, &rotated);

    let out = env.run(["switch"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Switched to Account-2 (b@example.com)\n"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains(KEYCHAIN_FOLLOWUP), "{}", out.stdout);

    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    assert_eq!(
        prev(&env, 1),
        Some(creds("1")),
        ".prev keeps the replaced generation"
    );
    assert_eq!(
        parse(&live(&env).unwrap()),
        parse(&creds("2")),
        "the target's stored login is now live"
    );

    let config = env.read_json(env.global_config_path());
    assert_eq!(config["oauthAccount"], oauth_account(2));
    assert_eq!(config["projects"], json!({"/work": {"allowedTools": []}}));
    let text = std::fs::read_to_string(env.global_config_path()).unwrap();
    assert!(text.contains("caf\\u00e9"), "ensure_ascii rewrite: {text}");
    assert_eq!(mode(&env.global_config_path()), 0o600);

    let outgoing_config = env
        .store_dir()
        .join("configs/.claude-config-1-a@example.com.json");
    assert_eq!(
        env.read_json(&outgoing_config)["oauthAccount"],
        oauth_account(1),
        "the outgoing config is captured before the splice"
    );
    let seq = sequence(&env);
    assert_eq!(seq["activeAccountNumber"], 2);
    assert_ne!(seq["lastUpdated"], "2024-01-01T00:00:00Z");
}

#[test]
fn an_untouched_outgoing_login_refreshes_only_the_config_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    env.run(["switch"]).assert_code(0);

    assert_eq!(backup_writes(&env, 1), 0, "{:?}", env.security().calls());
    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(prev(&env, 1), None);
    assert_eq!(sequence(&env)["activeAccountNumber"], 2);
}

#[test]
fn rotation_wraps_around_the_sequence() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 3);

    env.run(["switch"]).assert_code(0);

    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("1")));
}

// TestSwitchSkipsBrokenSlots
#[test]
fn rotation_skips_disabled_and_unswitchable_slots() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let mut seq = sequence(&env);
    seq["accounts"]["2"]["disabled"] = json!(true);
    env.seed_sequence(seq);

    let out = env.run(["switch"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Skipping Account-2 (disabled)\nSwitched to Account-3 (c@example.com)\n"),
        "{}",
        out.stdout
    );

    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    std::fs::remove_file(
        env.store_dir()
            .join("configs/.claude-config-2-b@example.com.json"),
    )
    .unwrap();
    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let report = out.json();
    assert_eq!(
        report["warnings"],
        json!(["Skipped Account-2 (no stored credentials/config)"])
    );
    assert_eq!(report["to"], json!({"number": 3, "email": "c@example.com"}));
}

#[test]
fn rotation_with_no_valid_target_stays_put() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut seq = sequence(&env);
    seq["accounts"]["2"]["disabled"] = json!(true);
    env.seed_sequence(seq);

    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let here = json!({"number": 1, "email": "a@example.com"});
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1, "switched": false, "from": here, "to": here,
            "strategy": "rotation", "reason": "no-valid-target",
            "message": "No other accounts have valid stored credentials/config.",
            "warnings": ["Skipped Account-2 (disabled)"],
        })
    );
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);

    let out = env.run(["switch"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Skipping Account-2 (disabled)\nNo other accounts have valid stored credentials/config.\nRe-add a skipped slot with: cswap --add-account --slot <number>\n"
    );
}

#[test]
fn switch_to_reaches_a_disabled_slot_when_named() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut seq = sequence(&env);
    seq["accounts"]["2"]["disabled"] = json!(true);
    env.seed_sequence(seq);

    env.run(["switch", "2"]).assert_code(0);

    assert_eq!(sequence(&env)["activeAccountNumber"], 2);
}

#[test]
fn switch_to_resolves_alias_and_email() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let mut seq = sequence(&env);
    seq["accounts"]["3"]["alias"] = json!("work");
    env.seed_sequence(seq);

    env.run(["switch", "WORK"]).assert_code(0);
    assert_eq!(sequence(&env)["activeAccountNumber"], 3);

    env.run(["switch", "b@example.com"]).assert_code(0);
    assert_eq!(sequence(&env)["activeAccountNumber"], 2);
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
}

// test_switch_to_result_no_leakage
#[test]
fn switch_to_json_reports_the_switch_and_nothing_else() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "{\n  \"schemaVersion\": 1,\n  \"switched\": true,\n  \"from\": {\n    \"number\": 1,\n    \"email\": \"a@example.com\"\n  },\n  \"to\": {\n    \"number\": 2,\n    \"email\": \"b@example.com\"\n  },\n  \"strategy\": \"direct\",\n  \"reason\": \"switched\",\n  \"message\": \"Switched to Account-2 (b@example.com)\",\n  \"warnings\": []\n}\n"
    );
    assert_eq!(sequence(&env)["activeAccountNumber"], 2);
}

#[test]
fn bare_switch_json_uses_the_rotation_strategy() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    let report = env.run(["switch", "--json"]).json();
    assert_eq!(report["strategy"], "rotation");
    assert_eq!(report["reason"], "switched");
    assert_eq!(report["switched"], true);
}

// test_switch_to_already_active_short_circuits
#[test]
fn switching_to_the_live_slot_is_a_noop() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    let out = env.run(["switch", "1", "--json"]);
    out.assert_code(0);
    let here = json!({"number": 1, "email": "a@example.com"});
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1, "switched": false, "from": here, "to": here,
            "strategy": "direct", "reason": "already-active",
            "message": "Already on Account-1 (a@example.com)", "warnings": [],
        })
    );

    let out = env.run(["switch", "1"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Already on Account-1 (a@example.com)\nTo rewrite the live login from the stored backup (e.g. after --import), run: cswap --switch-to 1 --force\n"
    );
    let writes = env
        .security()
        .calls()
        .into_iter()
        .filter(|c| c.argv == ["-i"])
        .count();
    assert_eq!(writes, 0);
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
}

#[test]
fn rotation_landing_on_the_live_slot_is_a_noop() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 2);
    let mut seq = sequence(&env);
    seq["activeAccountNumber"] = json!(1);
    env.seed_sequence(seq);

    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let report = out.json();
    assert_eq!(report["reason"], "already-active");
    assert_eq!(report["message"], "Already on Account-2 (b@example.com)");
}

// test_switch_only_one_account / test_noop_from_equals_to
#[test]
fn a_single_account_has_nowhere_to_go() {
    let env = TestEnv::new();
    seed(&env, &[1], 1);

    let report = env.run(["switch", "--json"]).json();
    let here = json!({"number": 1, "email": "a@example.com"});
    assert_eq!(report["switched"], false);
    assert_eq!(report["reason"], "only-one-account");
    assert_eq!(report["from"], here);
    assert_eq!(report["to"], here);

    let out = env.run(["switch"]);
    assert_eq!(
        out.stdout,
        "Only one account is managed. Add more accounts to switch between.\n"
    );
}

// test_switch_unmanaged_account_is_noop_without_add
#[test]
fn json_switch_from_an_unmanaged_login_changes_nothing() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.seed_login("stranger@example.com", "", "");

    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let unmanaged = json!({"number": null, "email": "stranger@example.com"});
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1, "switched": false, "from": unmanaged, "to": unmanaged,
            "strategy": "rotation", "reason": "unmanaged-account",
            "message": "Active account is not managed; run cswap --add-account",
            "warnings": [],
        })
    );
    assert_eq!(sequence(&env)["accounts"].as_object().unwrap().len(), 2);
}

// TestResolveIdentifierAmbiguity
#[test]
fn an_email_on_two_slots_asks_which_one_or_fails_in_json() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let mut seq = sequence(&env);
    for n in ["2", "3"] {
        seq["accounts"][n]["email"] = json!("shared@example.com");
    }
    seq["accounts"]["2"]["organizationName"] = json!("Acme");
    seq["accounts"]["2"]["organizationUuid"] = json!("org-acme");
    env.seed_sequence(seq);
    for n in [2, 3] {
        env.security().set_item(
            BACKUP_SERVICE,
            &format!("account-{n}-shared@example.com"),
            &creds(&n.to_string()),
        );
        env.write_file(
            env.store_dir().join(format!(
                "configs/.claude-config-{n}-shared@example.com.json"
            )),
            &json!({"oauthAccount": {"emailAddress": "shared@example.com"}}).to_string(),
        );
    }

    let out = env.run(["switch", "shared@example.com", "--json"]);
    out.assert_code(1);
    assert_eq!(
        out.json(),
        json!({"schemaVersion": 1, "error": {
            "type": "ConfigError",
            "message": "Email 'shared@example.com' is ambiguous — matches accounts: 2 [Acme], 3 [personal]. Use account number instead (e.g., cswap --switch-to 1).",
        }})
    );

    let out = env.cmd(["switch", "shared@example.com"]).stdin("7\n").run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Multiple accounts found for 'shared@example.com':\n  2: shared@example.com [Acme]\n  3: shared@example.com [personal]\nEnter account number to switch to: Cancelled\n"
    );
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);

    let out = env.cmd(["switch", "shared@example.com"]).stdin("3\n").run();
    out.assert_code(0);
    assert_eq!(sequence(&env)["activeAccountNumber"], 3);
}

#[test]
fn unknown_identifiers_are_reported() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    let error = |args: &[&str]| {
        let out = env.run(args);
        out.assert_code(1);
        out.json()["error"].clone()
    };
    assert_eq!(
        error(&["switch", "not-an-alias", "--json"]),
        json!({"type": "ValidationError", "message": "Invalid account identifier: not-an-alias"})
    );
    assert_eq!(
        error(&["switch", "nobody@example.com", "--json"]),
        json!({"type": "AccountNotFoundError", "message": "No account found with identifier: nobody@example.com"})
    );
    assert_eq!(
        error(&["switch", "9", "--json"]),
        json!({"type": "AccountNotFoundError", "message": "Account-9 does not exist"})
    );
}

#[test]
fn switching_without_a_store_fails() {
    let env = TestEnv::new();
    env.seed_login("a@example.com", "", "");
    let out = env.run(["switch", "--json"]);
    out.assert_code(1);
    assert_eq!(
        out.json()["error"],
        json!({"type": "ConfigError", "message": "No accounts are managed yet"})
    );
}

// TestSharedOAuthCredentialPreservation
#[test]
fn machine_shared_credential_fields_survive_the_switch() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut live_value = oauth("at-1", "rt-1");
    live_value["mcpOAuth"] = json!({"server": {"accessToken": "mcp-live"}});
    live_value["trustedDeviceToken"] = json!("device-1");
    set_live(&env, &live_value.to_string());
    let mut target = oauth("at-2", "rt-2");
    target["mcpOAuth"] = json!({"server": {"accessToken": "mcp-stale"}});
    target["pluginSecrets"] = json!({"p": "stale"});
    target["trustedDeviceToken"] = json!("device-2");
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(2), &target.to_string());

    env.run(["switch", "2"]).assert_code(0);

    let activated = parse(&live(&env).unwrap());
    let mut expected = oauth("at-2", "rt-2");
    expected["trustedDeviceToken"] = json!("device-2");
    expected["mcpOAuth"] = json!({"server": {"accessToken": "mcp-live"}});
    assert_eq!(
        activated, expected,
        "live shared keys win, absence included"
    );
    assert_eq!(
        parse(&backup(&env, 1).unwrap()),
        live_value,
        "the outgoing slot keeps its whole credential"
    );
}

// test_switch_refuses_to_overwrite_backup_with_empty_current_creds
#[test]
fn an_empty_live_credential_aborts_before_any_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, "");

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(1);
    let error = &out.json()["error"];
    assert_eq!(error["type"], "CredentialReadError");
    assert_eq!(
        error["message"],
        "Current account credential is empty (Keychain unreadable?); refusing to overwrite its backup"
    );
    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
}

#[test]
fn a_target_without_stored_credentials_fails_before_activation() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(2), "");

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(1);
    assert_eq!(
        out.json()["error"],
        json!({"type": "SwitchError", "message": "Account-2 has no stored credentials. Re-add with: cswap --add-account --slot 2"})
    );
    assert_eq!(live(&env), Some(creds("1")));
}

#[test]
fn a_failed_config_step_rolls_the_credentials_back() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-1-new", "rt-1").to_string();
    set_live(&env, &rotated);
    write_config_backup(&env, 2, "not json");
    let config_before = std::fs::read_to_string(env.global_config_path()).unwrap();

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(1);
    let error = &out.json()["error"];
    assert_eq!(error["type"], "SwitchError");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with("Switch failed and was rolled back: "),
        "{message}"
    );

    assert_eq!(live(&env).as_deref(), Some(rotated.as_str()));
    assert_eq!(
        std::fs::read_to_string(env.global_config_path()).unwrap(),
        config_before
    );
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
    assert_eq!(
        backup(&env, 1).as_deref(),
        Some(rotated.as_str()),
        "the outgoing backup is not undone"
    );
}

#[test]
fn a_failed_sequence_write_rolls_back_config_and_credentials() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let store = env.store_dir();
    env.write_file(store.join(".lock"), "");
    let config_before = std::fs::read_to_string(env.global_config_path()).unwrap();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = env.run(["switch", "2", "--json"]);
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    out.assert_code(1);
    let message = out.json()["error"]["message"].as_str().unwrap().to_string();
    assert!(
        message.starts_with("Switch failed and was rolled back: "),
        "{message}"
    );
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("1")));
    assert_eq!(
        std::fs::read_to_string(env.global_config_path()).unwrap(),
        config_before,
        "the original config bytes are restored"
    );
    assert_eq!(mode(&env.global_config_path()), 0o600);
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
}

// test_switch_holds_both_cc_locks_at_write_time
#[test]
fn claude_code_locks_are_held_while_the_live_credential_is_written() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let home = env.home();
    let refresh_lock = home.join(".claude/.oauth_refresh.lock");
    let credentials_lock = home.join(".claude.lock");
    let config_lock = home.join(".claude.json.lock");
    env.security()
        .observe(&[&refresh_lock, &credentials_lock, &config_lock]);

    env.run(["switch", "2"]).assert_code(0);

    let live_writes: Vec<_> = env
        .security()
        .calls()
        .into_iter()
        .filter(|c| {
            c.stdin
                .as_deref()
                .is_some_and(|s| s.contains(&format!("\"{ACTIVE_OAUTH_SERVICE}\"")))
        })
        .collect();
    assert_eq!(live_writes.len(), 1);
    assert!(
        live_writes[0].observed.values().all(|held| *held),
        "{:?}",
        live_writes[0].observed
    );
    for lock in [&refresh_lock, &credentials_lock, &config_lock] {
        assert!(!lock.exists(), "{} released", lock.display());
    }
}

// test_preheld_cc_lock_fails_cleanly_without_mutation
#[test]
fn a_held_refresh_lock_times_out_before_anything_is_mutated() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let refresh_lock = env.home().join(".claude/.oauth_refresh.lock");
    std::fs::create_dir(&refresh_lock).unwrap();
    let later_locks = [
        env.home().join(".claude.lock"),
        env.home().join(".claude.json.lock"),
    ];

    let child = {
        let paths = later_locks.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while std::time::Instant::now() < deadline {
                if paths.iter().any(|p| p.exists()) {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            true
        })
    };
    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(1);
    assert!(
        child.join().unwrap(),
        "later locks are taken only after the refresh lock"
    );
    assert_eq!(
        out.json()["error"],
        json!({"type": "ClaudeCodeLockTimeout",
               "message": "Could not acquire .oauth_refresh.lock — Claude Code appears to be refreshing credentials. Retry in a few seconds."})
    );
    assert_eq!(live(&env), Some(creds("1")));
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
    assert!(refresh_lock.is_dir(), "the holder's lock is left alone");
}

#[test]
fn a_file_fallback_says_the_switch_lands_on_the_next_message() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.security().fail_command("add-generic-password", 51);

    let out = env.run(["switch", "2"]);
    out.assert_code(0);
    assert!(out.stdout.contains(FILE_FOLLOWUP), "{}", out.stdout);
    let written = std::fs::read_to_string(env.home().join(".claude/.credentials.json")).unwrap();
    assert_eq!(parse(&written), parse(&creds("2")));
    assert_eq!(live(&env), None, "the stale Keychain item is removed");
}

// test_switch_back_to_oauth_clears_key
#[test]
fn switching_between_api_key_and_oauth_slots_keeps_one_auth_axis() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(2), key);

    env.run(["switch", "2"]).assert_code(0);

    assert_eq!(
        env.security()
            .item(MANAGED_KEY_SERVICE, KEYCHAIN_ACCOUNT)
            .as_deref(),
        Some(key)
    );
    assert_eq!(live(&env), None, "the OAuth login is cleared");
    let config = env.read_json(env.global_config_path());
    assert_eq!(
        config["customApiKeyResponses"],
        json!({"approved": [&key[key.len() - 20..]], "rejected": []})
    );
    assert!(config.get("primaryApiKey").is_none());

    env.run(["switch", "1"]).assert_code(0);

    assert_eq!(
        env.security().item(MANAGED_KEY_SERVICE, KEYCHAIN_ACCOUNT),
        None,
        "switching back to OAuth clears the managed key"
    );
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("1")));
    assert_eq!(backup(&env, 2).as_deref(), Some(key));
}

// test_session_shell_config_dir_refuses_switch
#[test]
fn a_session_shell_cannot_switch() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let session = env.store_dir().join("sessions/1-a");
    std::fs::create_dir_all(&session).unwrap();

    let out = env
        .cmd(["switch", "2", "--json"])
        .env("CLAUDE_CONFIG_DIR", &session.display().to_string())
        .run();
    out.assert_code(1);
    assert_eq!(out.json()["error"]["type"], "SwitchError");
    assert_eq!(sequence(&env)["activeAccountNumber"], 1);
}
