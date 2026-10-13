//! `auto --once` safety through the binary: the pre-switch refresh, the
//! quarantine lifecycle, the no-return bar, failover, the live-session skip,
//! API-key accounts as a last resort, and the known-foreign classification
//! a usage pass hands to the switch in the same process.
//!
//! Ported from upstream tests/test_autoswitch.py (TestFreshening,
//! TestQuarantineLifecycle, TestTokenIdentity, TestApiKeyAccounts,
//! TestDecisionTable failover rows) and tests/test_switcher.py
//! (test_cached_foreign_verdict_survives_a_failed_switch_time_probe), with
//! the real switcher, consume gate and stub endpoints in place of patched
//! methods.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, Output, StubResponse, TestEnv};

const EMAILS: [&str; 4] = ["", "a@example.com", "b@example.com", "c@example.com"];
const TOKEN_PATH: &str = "/v1/oauth/token";
const FAR: i64 = 9_999_999_999_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn now() -> f64 {
    now_ms() as f64 / 1000.0
}

fn creds_expiring(num: u32, expires_at: i64) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("at-{num}"),
        "refreshToken": format!("rt-{num}"),
        "expiresAt": expires_at,
    }})
    .to_string()
}

fn creds(num: u32) -> String {
    creds_expiring(num, FAR)
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": EMAILS[num as usize], "accountUuid": format!("uuid-{num}")})
}

fn backup_account(num: u32) -> String {
    format!("account-{num}-{}", EMAILS[num as usize])
}

/// Slots `nums` with stored credentials and configs; `live` is logged in.
fn seed(env: &TestEnv, nums: &[u32], live: u32) {
    let accounts: serde_json::Map<String, Value> = nums
        .iter()
        .map(|n| {
            let record = json!({"email": EMAILS[*n as usize], "uuid": format!("uuid-{n}"),
                                "organizationUuid": "", "organizationName": "",
                                "added": "2024-01-01T00:00:00Z"});
            (n.to_string(), record)
        })
        .collect();
    env.seed_sequence(json!({
        "activeAccountNumber": live,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": nums,
        "accounts": accounts,
    }));
    for n in nums {
        env.security()
            .set_item("claude-swap", &backup_account(*n), &creds(*n));
        env.write_file(
            env.store_dir().join(format!(
                "configs/.claude-config-{n}-{}.json",
                EMAILS[*n as usize]
            )),
            &json!({"oauthAccount": oauth_account(*n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(live)}),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds(live));
}

fn set_backup(env: &TestEnv, num: u32, value: &str) {
    env.security()
        .set_item("claude-swap", &backup_account(num), value);
}

fn backup(env: &TestEnv, num: u32) -> String {
    env.security()
        .item("claude-swap", &backup_account(num))
        .unwrap_or_default()
}

fn five_hour(pct: f64) -> Value {
    json!({"five_hour": {"utilization": pct}, "seven_day": {"utilization": 0.0}})
}

/// Usage per bearer token; unknown tokens answer 500.
fn stub_usage(env: &TestEnv, rows: &[(&str, f64)]) {
    let rows: Vec<(String, String)> = rows
        .iter()
        .map(|(token, pct)| (format!("Bearer {token}"), five_hour(*pct).to_string()))
        .collect();
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            let auth = req.header("Authorization").unwrap_or_default().to_string();
            rows.iter()
                .find(|(a, _)| *a == auth)
                .map(|(_, b)| StubResponse::json(200, b))
                .unwrap_or_else(|| StubResponse::json(500, "{}"))
        });
}

fn token_response(env: &TestEnv, status: u16, body: Value) {
    env.http().respond(
        "POST",
        TOKEN_PATH,
        StubResponse::json(status, &body.to_string()),
    );
}

fn rotated(account: Option<Value>) -> Value {
    let mut body =
        json!({"access_token": "at-2-new", "refresh_token": "rt-2-new", "expires_in": 3600});
    if let Some(account) = account {
        body["account"] = account;
    }
    body
}

fn token_posts(env: &TestEnv) -> Vec<Value> {
    env.http()
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path() == TOKEN_PATH)
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect()
}

fn events(out: &Output) -> Vec<Value> {
    out.stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn of_kind<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event"] == kind).collect()
}

fn live_token(env: &TestEnv) -> String {
    let live = env
        .security()
        .item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)
        .unwrap();
    serde_json::from_str::<Value>(&live).unwrap()["claudeAiOauth"]["accessToken"]
        .as_str()
        .unwrap()
        .to_string()
}

fn state(env: &TestEnv) -> Value {
    let path = env.store_dir().join("autoswitch_state.json");
    if path.exists() {
        env.read_json(path)
    } else {
        json!({})
    }
}

fn settings(env: &TestEnv, autoswitch: Value) {
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "autoswitch": autoswitch}),
    );
}

fn auto(env: &TestEnv) -> Output {
    env.run(["auto", "--once", "--json"])
}

/// Within the 10-minute freshen window, outside usage's 5-minute one.
fn near_expiry() -> i64 {
    now_ms() + 8 * 60 * 1000
}

#[test]
fn freshening_test_near_expiry_target_is_refreshed_and_persisted() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(&env, 200, rotated(None));

    let out = auto(&env);

    out.assert_code(0);
    let posts = token_posts(&env);
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0]["refresh_token"], "rt-2");
    assert_eq!(live_token(&env), "at-2-new");
    let stored: Value = serde_json::from_str(&backup(&env, 2)).unwrap();
    assert_eq!(stored["claudeAiOauth"]["refreshToken"], "rt-2-new");
}

#[test]
fn freshening_test_fresh_target_is_not_refreshed() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_backup(&env, 2, &creds_expiring(2, now_ms() + 3_600_000));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(&env, 200, rotated(None));

    auto(&env).assert_code(0);

    assert!(token_posts(&env).is_empty());
    assert_eq!(live_token(&env), "at-2");
}

#[test]
fn freshening_test_invalid_grant_quarantines_and_tries_next() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0), ("at-3", 20.0)]);
    token_response(&env, 400, json!({"error": "invalid_grant"}));

    let out = auto(&env);

    out.assert_code(0);
    let events = events(&out);
    let quarantined = of_kind(&events, "account-quarantined");
    assert_eq!(quarantined.len(), 1, "{}", out.stdout);
    assert_eq!(quarantined[0]["number"], "2");
    assert_eq!(quarantined[0]["email"], EMAILS[2]);
    assert_eq!(quarantined[0]["reason"], "invalid_grant");
    assert_eq!(of_kind(&events, "switch")[0]["to"]["number"], 3);
    assert_eq!(live_token(&env), "at-3");
    let entry = &state(&env)["quarantine"]["2"];
    assert_eq!(entry["reason"], "invalid_grant");
    assert_eq!(entry["email"], EMAILS[2]);
    assert!(
        entry["refreshTokenFingerprint"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

#[test]
fn freshening_test_transient_failure_skips_without_quarantine() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(&env, 503, json!({}));

    let out = auto(&env);

    out.assert_code(1);
    let events = events(&out);
    assert_eq!(
        of_kind(&events, "error")[0]["message"],
        "could not freshen any candidate (network?)"
    );
    assert_eq!(live_token(&env), "at-1");
    assert!(state(&env).get("quarantine").is_none());
}

#[test]
fn token_identity_test_conflicting_uuid_quarantines_instead_of_activating() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(
        &env,
        200,
        rotated(Some(
            json!({"uuid": "uuid-someone-else", "email_address": "x@example.com"}),
        )),
    );

    let out = auto(&env);

    out.assert_code(3);
    let events = events(&out);
    let quarantined = of_kind(&events, "account-quarantined");
    assert_eq!(quarantined[0]["number"], "2");
    assert_eq!(quarantined[0]["reason"], "identity-conflict");
    assert_eq!(live_token(&env), "at-1");
    assert_eq!(
        state(&env)["quarantine"]["2"]["reason"],
        "identity-conflict"
    );
}

#[test]
fn token_identity_test_blank_slot_uuid_is_backfilled_from_the_refresh() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut data = env.read_json(env.store_dir().join("sequence.json"));
    data["accounts"]["2"]["uuid"] = json!("");
    env.write_json(env.store_dir().join("sequence.json"), &data);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(
        &env,
        200,
        rotated(Some(
            json!({"uuid": "uuid-2-real", "email_address": EMAILS[2]}),
        )),
    );

    auto(&env).assert_code(0);

    let data = env.read_json(env.store_dir().join("sequence.json"));
    assert_eq!(data["accounts"]["2"]["uuid"], "uuid-2-real");
    assert_eq!(live_token(&env), "at-2-new");
}

#[test]
fn quarantine_lifecycle_test_replaced_credentials_lift_quarantine() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_backup(&env, 2, &creds_expiring(2, near_expiry()));
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0), ("at-2b", 10.0)]);
    token_response(&env, 400, json!({"error": "invalid_grant"}));
    auto(&env).assert_code(3);
    assert_eq!(state(&env)["quarantine"]["2"]["reason"], "invalid_grant");

    set_backup(
        &env,
        2,
        &json!({"claudeAiOauth": {"accessToken": "at-2b", "refreshToken": "rt-2b", "expiresAt": FAR}})
            .to_string(),
    );
    let out = auto(&env);

    out.assert_code(0);
    let events = events(&out);
    let released = of_kind(&events, "account-unquarantined");
    assert_eq!(released.len(), 1, "{}", out.stdout);
    assert_eq!(released[0]["number"], "2");
    assert_eq!(released[0]["reason"], "credentials-replaced");
    assert_eq!(live_token(&env), "at-2b");
    assert!(state(&env)["quarantine"].get("2").is_none());
}

#[test]
fn quarantine_lifecycle_test_quarantined_slot_is_never_a_candidate() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let fingerprint_of_2 =
        "sha256:1f23b7dadfb229cbadb8f2ab7d236f3b5192fb858ce87bf35e50d9d8e7dd9b38";
    env.write_json(
        env.store_dir().join("autoswitch_state.json"),
        &json!({"schemaVersion": 1, "quarantine": {"2": {
            "email": EMAILS[2], "reason": "invalid_grant", "at": "2024-01-01T00:00:00Z",
            "refreshTokenFingerprint": fingerprint_of_2}}}),
    );
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 0.0), ("at-3", 50.0)]);

    let out = auto(&env);

    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "switch")[0]["to"]["number"], 3);
    assert_eq!(live_token(&env), "at-3");
}

#[test]
fn live_session_target_is_skipped_even_with_fresh_token() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let profile = env.store_dir().join("sessions/2-b_example.com");
    std::fs::create_dir_all(profile.join("sessions")).unwrap();
    let pid = std::process::id();
    std::fs::write(
        profile.join("sessions").join(format!("{pid}.json")),
        json!({"pid": pid, "sessionId": "s", "cwd": "/w"}).to_string(),
    )
    .unwrap();
    stub_usage(&env, &[("at-1", 95.0), ("at-2", 10.0)]);
    token_response(&env, 200, rotated(None));

    let out = auto(&env);

    out.assert_code(3);
    let events = events(&out);
    assert_eq!(
        of_kind(&events, "no-switch")[0]["reason"],
        "no-viable-target"
    );
    assert!(token_posts(&env).is_empty());
    assert_eq!(live_token(&env), "at-1");
}

#[test]
fn unknown_active_usage_fails_over_after_unhealthy_ticks() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    settings(&env, json!({"unhealthyTicks": 1}));
    stub_usage(&env, &[("at-2", 10.0)]);

    let out = auto(&env);

    out.assert_code(0);
    let events = events(&out);
    assert_eq!(of_kind(&events, "switch")[0]["trigger"], "failover");
    assert_eq!(live_token(&env), "at-2");
    assert_eq!(state(&env)["leftTrigger"], "failover");
}

#[test]
fn unknown_active_usage_waits_below_unhealthy_ticks() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    stub_usage(&env, &[("at-2", 10.0)]);

    let out = auto(&env);

    out.assert_code(2);
    let events = events(&out);
    let held = of_kind(&events, "no-switch");
    assert_eq!(held[0]["reason"], "active-usage-unknown");
    assert_eq!(held[0]["detail"], "1/3 before failover");
    assert_eq!(live_token(&env), "at-1");
}

#[test]
fn api_key_accounts_test_api_key_is_last_resort_when_included() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let mut data = env.read_json(env.store_dir().join("sequence.json"));
    data["accounts"]["2"]["kind"] = json!("api_key");
    env.write_json(env.store_dir().join("sequence.json"), &data);
    set_backup(&env, 2, "sk-ant-api03-key");
    stub_usage(&env, &[("at-1", 95.0), ("at-3", 30.0)]);

    let out = env.run(["auto", "--once", "--json", "--include-api-key-accounts"]);

    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "switch")[0]["to"]["number"], 3);
}

#[test]
fn api_key_accounts_test_api_key_used_only_when_oauth_exhausted() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1);
    let mut data = env.read_json(env.store_dir().join("sequence.json"));
    data["accounts"]["2"]["kind"] = json!("api_key");
    env.write_json(env.store_dir().join("sequence.json"), &data);
    set_backup(&env, 2, "sk-ant-api03-key");
    stub_usage(&env, &[("at-1", 100.0), ("at-3", 100.0)]);

    env.run(["auto", "--once", "--json"]).assert_code(3);
    let out = env.run(["auto", "--once", "--json", "--include-api-key-accounts"]);

    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "switch")[0]["to"]["number"], 2);
}

/// Slot 2 is active after a proactive move off slot 1, which left 9 points
/// of headroom behind.
fn landed_on_two(env: &TestEnv) {
    seed(env, &[1, 2], 2);
    settings(env, json!({"hysteresisPct": 0}));
    env.write_json(
        env.store_dir().join("autoswitch_state.json"),
        &json!({"schemaVersion": 1, "lastSwitchAt": now() - 3600.0, "lastSwitchTo": "2",
                "lastSwitchFrom": 1, "leftHeadroom": 9.0, "leftRecoveryAt": null,
                "leftTrigger": "proactive"}),
    );
}

#[test]
fn no_return_bar_holds_until_the_left_account_recovers() {
    let env = TestEnv::new();
    landed_on_two(&env);
    stub_usage(&env, &[("at-1", 89.0), ("at-2", 95.0)]);

    let out = auto(&env);

    out.assert_code(3);
    assert!(
        of_kind(&events(&out), "switch").is_empty(),
        "{}",
        out.stdout
    );
    assert_eq!(live_token(&env), "at-2");
}

#[test]
fn no_return_bar_releases_once_the_left_account_recovered() {
    let env = TestEnv::new();
    landed_on_two(&env);
    stub_usage(&env, &[("at-1", 85.0), ("at-2", 95.0)]);

    let out = auto(&env);

    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "switch")[0]["to"]["number"], 1);
    assert_eq!(live_token(&env), "at-1");
}

/// The usage pass proves the live credential foreign (the profile endpoint
/// names another account); the switch it triggers cannot probe again (the
/// endpoint is down by then) and must still keep those bytes out of the
/// outgoing slot.
#[test]
fn cached_foreign_verdict_survives_a_failed_switch_time_probe() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    settings(&env, json!({"unhealthyTicks": 1}));
    let foreign = json!({"claudeAiOauth": {"accessToken": "at-x", "refreshToken": "rt-x",
                                           "expiresAt": FAR}})
    .to_string();
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &foreign);
    stub_usage(&env, &[("at-x", 10.0), ("at-2", 10.0)]);
    let probes = Arc::new(AtomicUsize::new(0));
    let seen = probes.clone();
    env.http()
        .respond_with("GET", "/api/oauth/profile", move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                StubResponse::json(
                    200,
                    &json!({"account": {"uuid": "uuid-x", "email": "x@example.com"},
                            "organization": {"uuid": "org-x"}})
                    .to_string(),
                )
            } else {
                StubResponse::json(500, "{}")
            }
        });

    let out = auto(&env);

    out.assert_code(0);
    let events = events(&out);
    let switch = of_kind(&events, "switch");
    assert_eq!(switch[0]["trigger"], "failover", "{}", out.stdout);
    assert!(
        switch[0]["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap()
            .contains("The live credential was previously identified as another account's.")),
        "{}",
        out.stdout
    );
    assert!(probes.load(Ordering::SeqCst) >= 2);
    assert_eq!(live_token(&env), "at-2");
    assert_eq!(backup(&env, 1), creds(1));
    let manifest = env.read_json(env.store_dir().join("credentials/.unclaimed-manifest.json"));
    let rows: Vec<&Value> = manifest["entries"].as_object().unwrap().values().collect();
    assert_eq!(rows.len(), 1, "{manifest}");
    assert_eq!(rows[0]["reason"], "known-foreign");
    assert_eq!(rows[0]["configSlot"], "1");
}
