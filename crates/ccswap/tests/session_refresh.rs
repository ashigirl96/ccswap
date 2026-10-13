//! Ported from upstream tests/test_session.py::TestBootstrap
//! (test_happy_path's rotation half, test_reuse_skips_refresh_and_writes,
//! test_refresh_failure_uses_stored_creds,
//! test_setup_token_account_skips_refresh_silently) and
//! TestAConsumedGrantIsNotSpentOnAProfileThatWonBootstrap
//! (test_an_unpersisted_successor_is_not_reported_as_stashed).

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv, is_upstream};

const BACKUP_SERVICE: &str = "claude-swap";
const TOKEN_PATH: &str = "/v1/oauth/token";

fn email(num: u32) -> String {
    ["", "a@example.com", "b@example.com"][num as usize].to_string()
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}"),
           "organizationUuid": format!("org-{num}"), "organizationName": "Acme"})
}

fn refreshable(num: u32) -> String {
    json!({"claudeAiOauth": {"accessToken": format!("at-{num}"), "refreshToken": format!("rt-{num}"),
                             "expiresAt": 9_999_999_999_000_i64}})
    .to_string()
}

fn setup_token(num: u32) -> String {
    json!({"claudeAiOauth": {"accessToken": format!("sk-ant-oat01-{num}"), "expiresAt": 0}})
        .to_string()
}

fn seed(env: &TestEnv, slot2: &str) {
    let accounts: serde_json::Map<String, Value> = (1..=2)
        .map(|n| {
            (
                n.to_string(),
                json!({"email": email(n), "uuid": format!("uuid-{n}"),
                       "organizationUuid": format!("org-{n}"), "organizationName": "Acme",
                       "added": "2024-01-01T00:00:00Z"}),
            )
        })
        .collect();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": accounts,
    }));
    for n in 1..=2 {
        let creds = if n == 2 {
            slot2.to_string()
        } else {
            refreshable(n)
        };
        env.security()
            .set_item(BACKUP_SERVICE, &format!("account-{n}-{}", email(n)), &creds);
        env.write_file(
            env.store_dir()
                .join(format!("configs/.claude-config-{n}-{}.json", email(n))),
            &json!({"oauthAccount": oauth_account(n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(1)}),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &refreshable(1));
    env.claude().respond(
        "auth status --json",
        &json!({"loggedIn": true, "authMethod": "claude.ai", "email": email(2), "orgId": "org-2"})
            .to_string(),
        0,
    );
}

fn token_ok(env: &TestEnv) {
    env.http().respond(
        "POST",
        TOKEN_PATH,
        StubResponse::json(
            200,
            &json!({"access_token": "at-2-new", "refresh_token": "rt-2-new", "expires_in": 3600})
                .to_string(),
        ),
    );
}

fn token_posts(env: &TestEnv) -> Vec<Value> {
    env.http()
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path() == TOKEN_PATH)
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect()
}

fn profile_creds(env: &TestEnv) -> PathBuf {
    env.store_dir()
        .join("sessions/2-b_example.com/.credentials.json")
}

fn oauth_of(text: &str) -> Value {
    serde_json::from_str::<Value>(text).unwrap()["claudeAiOauth"].clone()
}

fn backup(env: &TestEnv) -> String {
    env.security()
        .item(BACKUP_SERVICE, "account-2-b@example.com")
        .unwrap_or_default()
}

#[test]
fn bootstrap_test_happy_path_seeds_the_rotated_generation_and_persists_it() {
    let env = TestEnv::new();
    seed(&env, &refreshable(2));
    token_ok(&env);

    env.run(["run", "2"]).assert_code(0);

    let posts = token_posts(&env);
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["grant_type"], "refresh_token");
    assert_eq!(posts[0]["refresh_token"], "rt-2");
    let seeded = oauth_of(&std::fs::read_to_string(profile_creds(&env)).unwrap());
    assert_eq!(seeded["accessToken"], "at-2-new");
    assert_eq!(seeded["refreshToken"], "rt-2-new");
    let stored = oauth_of(&backup(&env));
    assert_eq!(stored["accessToken"], "at-2-new");
    assert_eq!(stored["refreshToken"], "rt-2-new");
}

#[test]
fn bootstrap_test_reuse_skips_refresh_and_writes() {
    let env = TestEnv::new();
    seed(&env, &refreshable(2));
    token_ok(&env);
    env.run(["run", "2"]).assert_code(0);
    let first = std::fs::read_to_string(profile_creds(&env)).unwrap();

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(token_posts(&env).len(), 1);
    assert_eq!(std::fs::read_to_string(profile_creds(&env)).unwrap(), first);
}

#[test]
fn bootstrap_test_refresh_failure_uses_stored_creds() {
    let env = TestEnv::new();
    seed(&env, &refreshable(2));
    env.http()
        .respond("POST", TOKEN_PATH, StubResponse::json(500, "{}"));

    let out = env.run(["run", "2"]);

    out.assert_code(0);
    assert_eq!(token_posts(&env).len(), 1);
    assert_eq!(
        std::fs::read_to_string(profile_creds(&env)).unwrap(),
        refreshable(2)
    );
    assert_eq!(backup(&env), refreshable(2));
    // Upstream's pre-exec stdout is lost under a pipe (execvpe, no flush).
    if !is_upstream() {
        assert!(
            out.stdout.contains(
                "Could not refresh the token for Account-2; continuing with the stored credentials."
            ),
            "{}",
            out.stdout
        );
    }
}

#[test]
fn bootstrap_test_setup_token_account_skips_refresh_silently() {
    let env = TestEnv::new();
    seed(&env, &setup_token(2));
    token_ok(&env);

    let out = env.run(["run", "2"]);

    out.assert_code(0);
    assert!(token_posts(&env).is_empty());
    assert!(!out.stdout.contains("Could not refresh"), "{}", out.stdout);
    assert_eq!(
        std::fs::read_to_string(profile_creds(&env)).unwrap(),
        setup_token(2)
    );
}

#[test]
fn bootstrap_test_dead_refresh_token_continues_with_the_stored_credentials() {
    let env = TestEnv::new();
    seed(&env, &refreshable(2));
    env.http().respond(
        "POST",
        TOKEN_PATH,
        StubResponse::json(400, r#"{"error": "invalid_grant"}"#),
    );

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(token_posts(&env).len(), 1);
    assert_eq!(backup(&env), refreshable(2));
    assert_eq!(
        std::fs::read_to_string(profile_creds(&env)).unwrap(),
        refreshable(2)
    );
}

/// Every backup write is refused, so the successor exists only in memory and seeding would hand Claude the spent generation.
#[test]
fn a_consumed_grant_test_an_unpersisted_successor_is_not_reported_as_stashed() {
    let env = TestEnv::new();
    seed(&env, &refreshable(2));
    let credentials_dir = env.store_dir().join("credentials");
    std::fs::create_dir_all(&credentials_dir).unwrap();
    let lock_dir = credentials_dir.clone();
    env.http().respond_with("POST", TOKEN_PATH, move |_| {
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        StubResponse::json(
            200,
            &json!({"access_token": "at-2-new", "refresh_token": "rt-2-new", "expires_in": 3600})
                .to_string(),
        )
    });
    env.security().fail_command("add-generic-password", 51);

    let out = env.run(["run", "2"]);
    std::fs::set_permissions(&credentials_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert_ne!(out.code, 0, "{}{}", out.stdout, out.stderr);
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(
        text.contains("could neither be stored nor stashed"),
        "{text}"
    );
    assert!(!text.contains("The successor is stashed"), "{text}");
    assert!(!profile_creds(&env).exists());
    assert_eq!(backup(&env), refreshable(2));
}
