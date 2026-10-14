//! Ported from upstream tests/test_json_output.py::TestStatusJson (no_active,
//! unmanaged, managed, managed_includes_alias), TestJsonHelpers::
//! test_error_envelope_shape, tests/test_cli.py::TestJsonOutputCli::
//! test_error_envelope_on_stdout_with_exit_1, test_switcher.py::TestStatus and
//! the status branch of switcher.py:5546-5582. Usage numbers are tested in
//! list.rs, so these cover only verdicts that need no network.

mod support;

use serde_json::json;
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, MANAGED_KEY_SERVICE, TestEnv};

fn two_accounts() -> serde_json::Value {
    json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {
                "email": "user@example.com",
                "uuid": "user-uuid",
                "organizationUuid": "org-uuid-5678",
                "organizationName": "Acme Corp",
                "added": "2024-01-01T00:00:00Z"
            },
            "2": {
                "email": "user@example.com",
                "uuid": "user-uuid",
                "organizationUuid": "",
                "organizationName": "",
                "alias": "home",
                "added": "2024-01-02T00:00:00Z"
            }
        }
    })
}

#[test]
fn no_login_reports_no_active_account() {
    let env = TestEnv::new();

    let out = env.run(["status"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Status: No active Claude account\n");

    let out = env.run(["status", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "{\n  \"schemaVersion\": 1,\n  \"active\": null\n}\n"
    );
}

#[test]
fn login_without_a_store_is_not_managed() {
    let env = TestEnv::new();
    env.seed_login("test@example.com", "", "");

    let out = env.run(["status"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Status: test@example.com (not managed)\n");

    let out = env.run(["status", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "{\n  \"schemaVersion\": 1,\n  \"active\": {\n    \"email\": \"test@example.com\",\n    \"managed\": false\n  }\n}\n"
    );
}

#[test]
fn login_from_an_unregistered_org_is_not_managed() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "other-org", "Other");

    let out = env.run(["status", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.json(),
        json!({"schemaVersion": 1, "active": {"email": "user@example.com", "managed": false}})
    );
}

#[test]
fn managed_org_account_shows_slot_email_and_org() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");

    let out = env.run(["status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Status: Account-1 (user@example.com [Acme Corp])\n  Total managed accounts: 2\n  no credentials\n"
    );

    let out = env.run(["status", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        r#"{
  "schemaVersion": 1,
  "active": {
    "number": 1,
    "email": "user@example.com",
    "organizationName": "Acme Corp",
    "organizationUuid": "org-uuid-5678",
    "isOrganization": true,
    "managed": true,
    "usageStatus": "no_credentials",
    "usage": null
  },
  "totalManagedAccounts": 2
}
"#
    );
}

#[test]
fn managed_personal_account_shows_personal_tag_and_alias() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "", "");

    let out = env.run(["status"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Status: Account-2 (user@example.com [personal])\n"),
        "{}",
        out.stdout
    );

    let out = env.run(["status", "--json"]);
    let active = &out.json()["active"];
    assert_eq!(active["number"], 2);
    assert_eq!(active["isOrganization"], false);
    assert_eq!(active["alias"], "home");
    let keys: Vec<&str> = active
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "number",
            "email",
            "organizationName",
            "organizationUuid",
            "isOrganization",
            "managed",
            "usageStatus",
            "usage",
            "alias"
        ]
    );
}

#[test]
fn status_reads_the_active_credential_through_security() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");

    env.run(["status"]).assert_code(0);

    let argvs: Vec<Vec<String>> = env.security().calls().into_iter().map(|c| c.argv).collect();
    let expected: Vec<String> = [
        "find-generic-password",
        "-a",
        KEYCHAIN_ACCOUNT,
        "-w",
        "-s",
        ACTIVE_OAUTH_SERVICE,
    ]
    .map(String::from)
    .to_vec();
    assert!(argvs.contains(&expected), "{argvs:?}");
}

#[test]
fn managed_api_key_login_reports_api_key() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");
    env.security()
        .set_item(MANAGED_KEY_SERVICE, KEYCHAIN_ACCOUNT, "sk-ant-api03-test");

    let out = env.run(["status"]);
    out.assert_code(0);
    assert!(
        out.stdout.ends_with("\n  API key (no quota)\n"),
        "{}",
        out.stdout
    );

    let out = env.run(["status", "--json"]);
    assert_eq!(out.json()["active"]["usageStatus"], "api_key");
}

#[test]
fn locked_keychain_reports_keychain_unavailable() {
    let env = TestEnv::new();
    env.seed_sequence(two_accounts());
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");
    env.security().fail_with(36);

    let out = env.run(["status"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .ends_with("\n  keychain unavailable — locked or in use; try again\n"),
        "{}",
        out.stdout
    );

    let out = env.run(["status", "--json"]);
    assert_eq!(out.json()["active"]["usageStatus"], "keychain_unavailable");
}

#[test]
fn unreadable_store_emits_error_envelope_on_stdout() {
    let env = TestEnv::new();
    let sequence = env.seed_sequence(json!({}));
    std::fs::write(&sequence, "{ torn").unwrap();
    env.seed_login("user@example.com", "org-uuid-5678", "Acme Corp");

    let out = env.run(["status", "--json"]);
    out.assert_code(1);
    let payload = out.json();
    let keys: Vec<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["schemaVersion", "error"]);
    assert_eq!(payload["schemaVersion"], 1);
    assert_eq!(payload["error"]["type"], "ConfigError");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "{} exists but could not be parsed (",
            sequence.display()
        )),
        "{message}"
    );
    assert!(
        out.stdout.starts_with(
            "{\n  \"schemaVersion\": 1,\n  \"error\": {\n    \"type\": \"ConfigError\","
        )
    );

    let out = env.run(["status"]);
    out.assert_code(1);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.starts_with(&format!(
            "Error: {} exists but could not be parsed (",
            sequence.display()
        )),
        "{}",
        out.stderr
    );
}

#[test]
fn legacy_flag_and_unambiguous_prefix_reach_status() {
    let env = TestEnv::new();
    for args in [vec!["--status"], vec!["--stat"], vec!["status", "--js"]] {
        let out = env.run(&args);
        out.assert_code(0);
        assert!(
            out.stdout.contains("No active Claude account")
                || out.stdout.contains("\"active\": null"),
            "{args:?}: {}",
            out.stdout
        );
    }
}
