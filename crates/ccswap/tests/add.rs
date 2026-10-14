//! Ported from upstream tests/test_add_account_identity.py (ownership guard,
//! drift rechecks), test_switcher.py::TestAddAccountRefresh / TestAddAccountSlot
//! / TestAddAccountAlias / TestAddAccountOrgFields / TestAliasValidation,
//! TestRemoveAccountPrunesMappings::test_slot_overwrite_prunes_displaced_mappings,
//! test_api_key_accounts.py::TestAddAccountGuard /
//! TestATornRosterDoesNotDestroyBackups, test_macos_keychain.py (set_password
//! stdin vs argv) and test_macos_keychain_contract.py::TestBackupCredentialsSecurity.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn creds(tag: &str) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("sk-ant-oat01-{tag}"),
        "refreshToken": format!("sk-ant-ort01-{tag}"),
        "expiresAt": 9_999_999_999_000_i64,
    }})
    .to_string()
}

fn profile_body(uuid: &str, email: &str, org: Option<&str>) -> String {
    let mut body = json!({"account": {"uuid": uuid, "email": email}});
    if let Some(org) = org {
        body["organization"] = json!({"uuid": org});
    }
    body.to_string()
}

fn logged_in(env: &TestEnv, email: &str, org: &str, org_name: &str, tag: &str) {
    env.seed_login(email, org, org_name);
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds(tag));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &profile_body(&format!("uuid-{email}"), email, Some(org)),
        ),
    );
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn backup_account(num: u32, email: &str) -> String {
    format!("account-{num}-{email}")
}

fn assert_timestamp(value: &Value) {
    let s = value.as_str().expect("timestamp string");
    assert_eq!(s.len(), 20, "{s}");
    assert!(s.ends_with('Z') && s.as_bytes()[10] == b'T', "{s}");
}

fn seed_two(env: &TestEnv) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 3],
        "accounts": {
            "1": {"email": "one@example.com", "uuid": "u1", "organizationUuid": "",
                  "organizationName": "", "added": "2024-01-01T00:00:00Z"},
            "3": {"email": "three@example.com", "uuid": "u3", "organizationUuid": "",
                  "organizationName": "", "alias": "work", "added": "2024-01-01T00:00:00Z"}
        }
    }));
}

#[test]
fn first_add_creates_the_store_and_backs_up_into_the_keychain() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "org-1", "Acme", "A");

    let out = env.run(["add"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Added Account 1: a@example.com [Acme]\n");

    let store = env.store_dir();
    for dir in [
        store.clone(),
        store.join("configs"),
        store.join("credentials"),
    ] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    let seq_path = store.join("sequence.json");
    assert_eq!(mode(&seq_path), 0o600);
    let text = std::fs::read_to_string(&seq_path).unwrap();
    assert!(
        !text.ends_with('\n'),
        "json.dumps writes no trailing newline"
    );
    let seq = env.sequence();
    let keys: Vec<&str> = seq
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["activeAccountNumber", "lastUpdated", "sequence", "accounts"]
    );
    assert_eq!(seq["activeAccountNumber"], 1);
    assert_eq!(seq["sequence"], json!([1]));
    assert_timestamp(&seq["lastUpdated"]);
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
        ]
    );
    assert_eq!(record["email"], "a@example.com");
    assert_eq!(record["uuid"], "uuid-a@example.com");
    assert_eq!(record["organizationUuid"], "org-1");
    assert_eq!(record["organizationName"], "Acme");
    assert_timestamp(&record["added"]);

    let config = store.join("configs/.claude-config-1-a@example.com.json");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        std::fs::read_to_string(env.global_config_path()).unwrap(),
        "the config backup is a verbatim copy"
    );
    assert_eq!(mode(&config), 0o600);

    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(1, "a@example.com"))
            .as_deref(),
        Some(creds("A").as_str())
    );
    assert!(
        !store
            .join("credentials/.creds-1-a@example.com.enc")
            .exists()
    );
}

#[test]
fn add_verifies_ownership_through_the_profile_endpoint() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "org-1", "Acme", "A");

    env.run(["add"]).assert_code(0);

    let reqs: Vec<_> = env
        .http()
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/api/oauth/profile")
        .collect();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(
        reqs[0].header("Authorization"),
        Some("Bearer sk-ant-oat01-A")
    );
    assert_eq!(reqs[0].header("User-Agent"), Some("claude-swap/1.0"));
    assert_eq!(reqs[0].header("Content-Type"), Some("application/json"));
}

#[test]
fn the_secret_never_appears_in_argv() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "SECRET");

    env.run(["add"]).assert_code(0);

    let secret = creds("SECRET");
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    let calls = env.security().calls();
    let writes: Vec<_> = calls.iter().filter(|c| c.argv == ["-i"]).collect();
    assert_eq!(writes.len(), 1, "{calls:?}");
    assert_eq!(
        writes[0].stdin.as_deref(),
        Some(
            format!(
                "add-generic-password -U -a \"account-1-a@example.com\" -s \"claude-swap\" -X {hex}\n"
            )
            .as_str()
        )
    );
    for call in &calls {
        for arg in &call.argv {
            assert!(!arg.contains("SECRET") && !arg.contains(&hex), "{arg}");
        }
    }
}

#[test]
fn a_credential_too_long_for_the_stdin_line_falls_back_to_argv() {
    let env = TestEnv::new();
    let big = "x".repeat(2100);
    logged_in(&env, "a@example.com", "", "", &big);

    env.run(["add"]).assert_code(0);

    let secret = creds(&big);
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    let calls = env.security().calls();
    let write = calls
        .iter()
        .find(|c| c.argv.first().map(String::as_str) == Some("add-generic-password"))
        .expect("argv write");
    assert_eq!(
        write.argv,
        [
            "add-generic-password",
            "-U",
            "-a",
            "account-1-a@example.com",
            "-s",
            "claude-swap",
            "-X",
            hex.as_str()
        ]
    );
    assert!(calls.iter().all(|c| c.argv != ["-i"]));
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(1, "a@example.com"))
            .as_deref(),
        Some(secret.as_str())
    );
}

#[test]
fn add_takes_the_number_after_the_highest_slot() {
    let env = TestEnv::new();
    seed_two(&env);
    logged_in(&env, "new@example.com", "", "", "N");

    let out = env.run(["add"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Added Account 4: new@example.com [personal]\n");
    let seq = env.sequence();
    assert_eq!(seq["sequence"], json!([1, 3, 4]));
    assert_eq!(seq["activeAccountNumber"], 4);
    assert_eq!(
        seq["accounts"]["3"]["alias"], "work",
        "other records untouched"
    );
}

#[test]
fn readding_the_same_account_refreshes_its_slot_in_place() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "org-1", "Acme", "OLD");
    env.run(["add"]).assert_code(0);
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds("NEW"));

    let out = env.run(["add"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Updated credentials for Account 1 (a@example.com [Acme]).\n"
    );
    let seq = env.sequence();
    assert_eq!(seq["sequence"], json!([1]));
    assert_eq!(seq["accounts"].as_object().unwrap().len(), 1);
    let sec = env.security();
    assert_eq!(
        sec.item(BACKUP_SERVICE, &backup_account(1, "a@example.com"))
            .as_deref(),
        Some(creds("NEW").as_str())
    );
    assert_eq!(
        sec.item(
            BACKUP_SERVICE,
            &format!("{}.prev", backup_account(1, "a@example.com"))
        )
        .as_deref(),
        Some(creds("OLD").as_str()),
        "the replaced generation is retained as .prev in the Keychain"
    );
}

#[test]
fn slot_flag_registers_into_an_empty_slot() {
    let env = TestEnv::new();
    seed_two(&env);
    logged_in(&env, "new@example.com", "", "", "N");

    let out = env.run(["add", "--slot", "2"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Added Account 2: new@example.com [personal]\n");
    assert_eq!(env.sequence()["sequence"], json!([1, 2, 3]));
}

#[test]
fn slot_flag_below_one_is_refused() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "A");

    let out = env.run(["add", "--slot", "0"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Slot number must be >= 1\n");
}

#[test]
fn an_occupied_slot_asks_before_overwriting_and_no_keeps_it() {
    let env = TestEnv::new();
    seed_two(&env);
    let before = std::fs::read_to_string(env.store_dir().join("sequence.json")).unwrap();
    logged_in(&env, "new@example.com", "", "", "N");

    let out = env.cmd(["add", "--slot", "3"]).stdin("n\n").run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Slot 3 already occupied\nthree@example.com [personal]\nOverwrite slot 3? [y/N] Cancelled\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.store_dir().join("sequence.json")).unwrap(),
        before
    );

    let out = env.cmd(["add", "--slot", "3"]).run();
    out.assert_code(0);
    assert!(
        out.stdout
            .ends_with("Overwrite slot 3? [y/N] \nCancelled\n"),
        "EOF cancels: {:?}",
        out.stdout
    );
}

#[test]
fn an_occupied_slot_is_replaced_after_yes() {
    let env = TestEnv::new();
    seed_two(&env);
    let store = env.store_dir();
    env.write_file(
        store.join("configs/.claude-config-3-three@example.com.json"),
        "{}",
    );
    env.security().set_item(
        BACKUP_SERVICE,
        &backup_account(3, "three@example.com"),
        &creds("THREE"),
    );
    env.write_json(
        store.join("mappings.json"),
        &json!({"schemaVersion": 1, "mappings": {
            "/w/three": {"email": "three@example.com", "organizationUuid": "", "added": "2024-01-01T00:00:00Z"},
            "/w/one": {"email": "one@example.com", "organizationUuid": "", "added": "2024-01-01T00:00:00Z"}
        }}),
    );
    logged_in(&env, "new@example.com", "", "", "N");

    let out = env.cmd(["add", "--slot", "3"]).stdin("y\n").run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Slot 3 already occupied\nthree@example.com [personal]\nOverwrite slot 3? [y/N] \
         Removed 1 directory mapping(s) for this account\n\
         Added Account 3: new@example.com [personal]\n"
    );
    let seq = env.sequence();
    assert_eq!(seq["sequence"], json!([1, 3]));
    assert_eq!(seq["accounts"]["3"]["email"], "new@example.com");
    assert!(
        seq["accounts"]["3"].get("alias").is_none(),
        "the displaced account's alias does not carry"
    );
    let keys: Vec<&str> = seq["accounts"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["1", "3"]);
    assert!(
        !store
            .join("configs/.claude-config-3-three@example.com.json")
            .exists()
    );
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(3, "three@example.com")),
        None
    );
    let mappings = env.read_json(store.join("mappings.json"));
    assert_eq!(
        mappings["mappings"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["/w/one"]
    );
    assert_eq!(mode(&store.join("mappings.json")), 0o600);
}

#[test]
fn slot_flag_moves_the_same_account_and_carries_its_alias() {
    let env = TestEnv::new();
    seed_two(&env);
    env.security().set_item(
        BACKUP_SERVICE,
        &backup_account(3, "three@example.com"),
        &creds("OLD"),
    );
    logged_in(&env, "three@example.com", "", "", "T");

    let out = env.run(["add", "--slot", "5"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Moved from slot 3 → 5\nAdded Account 5: three@example.com [personal]\n"
    );
    let seq = env.sequence();
    assert_eq!(seq["sequence"], json!([1, 5]));
    assert!(seq["accounts"].get("3").is_none());
    assert_eq!(seq["accounts"]["5"]["alias"], "work");
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(3, "three@example.com")),
        None
    );
}

#[test]
fn alias_flag_sets_a_normalized_alias() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "A");

    env.run(["add", "--alias", "  Dev.Box_1 "]).assert_code(0);
    assert_eq!(env.sequence()["accounts"]["1"]["alias"], "dev.box_1");
}

#[test]
fn invalid_aliases_are_refused_before_anything_is_written() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "A");

    for (alias, message) in [
        (
            "123",
            "alias '123' cannot be purely numeric (reserved for slot numbers)",
        ),
        (
            "-x",
            "alias '-x' cannot start with '-' (would be read as a command flag)",
        ),
        (
            "has space",
            "alias 'has space' may only contain letters, digits, '-', '_', and '.'",
        ),
        ("  ", "alias cannot be empty"),
    ] {
        let out = env.run(["add", &format!("--alias={alias}")]);
        out.assert_code(1);
        assert_eq!(out.stderr, format!("Error: {message}\n"));
        let out = env.run(["add", "--json", &format!("--alias={alias}")]);
        out.assert_code(2);
    }
    let seq = env.sequence();
    assert_eq!(seq["accounts"], json!({}));
}

#[test]
fn an_alias_held_by_another_account_is_refused() {
    let env = TestEnv::new();
    seed_two(&env);
    logged_in(&env, "new@example.com", "", "", "N");

    let out = env.run(["add", "--alias", "WORK"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: Alias 'work' is already used by account 3\n"
    );
    assert!(env.sequence()["accounts"].get("4").is_none());
}

#[test]
fn no_login_is_an_error() {
    let env = TestEnv::new();

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: No active Claude account found. Please log in first.\n"
    );
}

#[test]
fn a_login_without_credentials_is_an_error() {
    let env = TestEnv::new();
    env.seed_login("a@example.com", "", "");

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: No credentials found for current account\n"
    );
}

#[test]
fn a_live_api_key_login_is_refused() {
    let env = TestEnv::new();
    env.seed_login("a@example.com", "", "");
    env.security().set_item(
        support::MANAGED_KEY_SERVICE,
        KEYCHAIN_ACCOUNT,
        "sk-ant-api03-live",
    );

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: Active login is an API-key account. Add it with 'ccswap --add-token sk-ant-api...' instead of --add-account.\n"
    );
}

#[test]
fn a_torn_roster_is_refused_instead_of_overwritten() {
    let env = TestEnv::new();
    seed_two(&env);
    let path = env.write_file(env.store_dir().join("sequence.json"), "{\"accounts\": ");
    logged_in(&env, "a@example.com", "", "", "A");

    let out = env.run(["add"]);
    out.assert_code(1);
    assert!(
        out.stderr.contains("exists but could not be parsed"),
        "{}",
        out.stderr
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{\"accounts\": ");
}

#[test]
fn a_credential_owned_by_another_account_is_refused() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "", "", "THEIRS");
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile_body("u-other", "other@example.com", None)),
    );

    let out = env.run(["add", "--slot", "7"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: The stored credential does not belong to ax@example.com: the token resolves to account u-other, not uuid-ax@example.com. Nothing was changed. This happens when the config names one account while the credential store still holds another's token (e.g. a renamed .claude.json over a live keychain item). Log in as ax@example.com in THIS environment, then re-run.\n"
    );
    assert!(env.sequence()["accounts"].get("7").is_none());
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(7, "ax@example.com")),
        None
    );
}

#[test]
fn without_an_account_uuid_the_email_decides() {
    let env = TestEnv::new();
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": {"emailAddress": "ax@example.com", "organizationUuid": ""}}),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds("T"));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile_body("u-x", "other@example.com", Some(""))),
    );

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: The stored credential does not belong to ax@example.com: the token resolves to other@example.com. Nothing was changed. This happens when the config names one account while the credential store still holds another's token (e.g. a renamed .claude.json over a live keychain item). Log in as ax@example.com in THIS environment, then re-run.\n"
    );
}

#[test]
fn a_matching_uuid_under_a_different_org_is_refused() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "", "", "T");
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &profile_body("uuid-ax@example.com", "ax@example.com", Some("org-X")),
        ),
    );

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: The stored credential for ax@example.com belongs to organization org-X, not personal. Nothing was changed. Two accounts can share an email across organizations. Log in as ax@example.com in the personal organization in THIS environment, then re-run.\n"
    );
}

#[test]
fn a_profile_without_an_org_block_registers_an_org_slot() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "org-1", "Acme", "T");
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &profile_body("uuid-ax@example.com", "ax@example.com", None),
        ),
    );

    env.run(["add"]).assert_code(0);
    assert_eq!(env.sequence()["accounts"]["1"]["organizationUuid"], "org-1");
}

#[test]
fn an_unresolvable_profile_registers_with_a_notice() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "", "", "T");
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(401, r#"{"error":"unauthorized"}"#),
    );

    let out = env.run(["add"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Notice: could not verify that the stored credential belongs to ax@example.com (the identity lookup did not resolve). Registering anyway; re-run where the check can complete to confirm.\nAdded Account 1: ax@example.com [personal]\n"
    );
}

#[test]
fn an_expired_access_token_is_never_sent_to_the_profile_endpoint() {
    let env = TestEnv::new();
    env.seed_login("ax@example.com", "", "");
    env.security().set_item(
        ACTIVE_OAUTH_SERVICE,
        KEYCHAIN_ACCOUNT,
        r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat01-EXPIRED", "refreshToken": "rt", "expiresAt": 1}}"#,
    );

    let out = env.run(["add"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Notice: could not verify that the stored credential belongs to ax@example.com (the access token is expired)."),
        "{}",
        out.stdout
    );
    assert!(
        env.http()
            .requests()
            .iter()
            .all(|r| r.path() != "/api/oauth/profile" && r.path() != "/v1/oauth/token"),
        "no profile fetch, no refresh"
    );
}

#[test]
fn a_login_landing_during_the_profile_lookup_is_refused() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "", "", "T");
    let config = env.global_config_path();
    env.http()
        .respond_with("GET", "/api/oauth/profile", move |_| {
            std::fs::write(
                &config,
                json!({"oauthAccount": {"emailAddress": "someone-else@example.com",
                   "organizationUuid": "", "accountUuid": "u-else"}})
                .to_string(),
            )
            .unwrap();
            StubResponse::json(
                200,
                &profile_body("uuid-ax@example.com", "ax@example.com", Some("")),
            )
        });

    let out = env.run(["add", "--slot", "7"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: The active account changed while ax@example.com was being verified (now someone-else@example.com). Nothing was changed. Re-run when no other login is in flight.\n"
    );
    assert!(env.sequence()["accounts"].get("7").is_none());
}

#[test]
fn a_refresh_landing_during_the_profile_lookup_is_refused() {
    let env = TestEnv::new();
    logged_in(&env, "ax@example.com", "", "", "OLD");
    let items = env
        .security()
        .path()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("state/items.json");
    let rotated = json!([{
        "service": ACTIVE_OAUTH_SERVICE, "account": KEYCHAIN_ACCOUNT, "password": creds("NEW")
    }])
    .to_string();
    env.http()
        .respond_with("GET", "/api/oauth/profile", move |_| {
            std::fs::write(&items, &rotated).unwrap();
            StubResponse::json(
                200,
                &profile_body("uuid-ax@example.com", "ax@example.com", Some("")),
            )
        });

    let out = env.run(["add"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: The stored credential rotated while it was being verified. Nothing was changed. Registering the pre-rotation generation would hand the slot a credential the server has already retired. Re-run when no refresh is in flight.\n"
    );
    assert_eq!(env.sequence()["accounts"], json!({}));
}

#[test]
fn a_refused_keychain_write_falls_back_to_a_private_enc_file() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "A");
    env.security().fail_command("add-generic-password", 51);

    env.run(["add"]).assert_code(0);

    let enc = env
        .store_dir()
        .join("credentials/.creds-1-a@example.com.enc");
    assert_eq!(mode(&enc), 0o600);
    assert_eq!(
        std::fs::read_to_string(&enc).unwrap(),
        base64(creds("A").as_bytes())
    );
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, &backup_account(1, "a@example.com")),
        None
    );
}

#[test]
fn an_enc_backup_wins_over_the_keychain_copy() {
    let env = TestEnv::new();
    logged_in(&env, "a@example.com", "", "", "A");
    env.run(["add"]).assert_code(0);
    env.write_file(
        env.store_dir()
            .join("credentials/.creds-1-a@example.com.enc"),
        &base64(creds("FROM-FILE").as_bytes()),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds("NEW"));

    env.run(["add"]).assert_code(0);

    let sec = env.security();
    assert_eq!(
        sec.item(
            BACKUP_SERVICE,
            &format!("{}.prev", backup_account(1, "a@example.com"))
        )
        .as_deref(),
        Some(creds("FROM-FILE").as_str()),
        "the generation read back (and retained) is the .enc one"
    );
    assert!(
        !env.store_dir()
            .join("credentials/.creds-1-a@example.com.enc")
            .exists(),
        "a successful Keychain write reconciles the .enc away"
    );
}

#[test]
fn add_inside_a_session_profile_is_refused() {
    let env = TestEnv::new();
    let session = env.store_dir().join("sessions/1-a");
    std::fs::create_dir_all(&session).unwrap();
    let out = env
        .cmd(["add"])
        .env("CLAUDE_CONFIG_DIR", session.to_str().unwrap())
        .run();
    out.assert_code(1);
    assert!(
        out.stderr
            .starts_with("Error: This shell is inside a cswap run session profile"),
        "{}",
        out.stderr
    );
}

#[test]
fn json_is_not_accepted_with_add() {
    let env = TestEnv::new();
    let out = env.run(["add", "--json"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .ends_with("error: --json can only be used with 'list', 'status', or 'switch'\n")
    );
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
