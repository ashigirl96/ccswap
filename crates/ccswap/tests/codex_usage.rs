mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{CodexAuth, StubResponse, TestEnv};

const USAGE_PATH: &str = "/backend-api/wham/usage";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn usage_body(five_pct: f64, five_reset: i64, week_pct: f64, week_reset: i64) -> String {
    json!({
        "plan_type": "plus",
        "rate_limit": {
            "primary_window": { "used_percent": five_pct, "reset_at": five_reset, "limit_window_seconds": 18000 },
            "secondary_window": { "used_percent": week_pct, "reset_at": week_reset, "limit_window_seconds": 604800 },
        },
    })
    .to_string()
}

/// Answers by the request's `ChatGPT-Account-Id`; unknown accounts get a 401.
fn stub_usage_by_account(env: &TestEnv, by_account: Vec<(&'static str, StubResponse)>) {
    env.http().respond_with("GET", USAGE_PATH, move |req| {
        let account = req.header("ChatGPT-Account-Id").unwrap_or_default();
        by_account
            .iter()
            .find(|(id, _)| *id == account)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| StubResponse::json(401, "{}"))
    });
}

fn usage_requests(env: &TestEnv) -> usize {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == USAGE_PATH)
        .count()
}

fn cache_path(env: &TestEnv) -> std::path::PathBuf {
    env.codex_store_dir().join("cache/usage.json")
}

fn cache_row(env: &TestEnv, number: u32) -> Value {
    env.read_json(cache_path(env))["accounts"][number.to_string()].clone()
}

#[test]
fn list_shows_each_windows_percent_and_time_to_reset() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a").plan("plus"));
    env.seed_codex_login(&CodexAuth::new("a@example.com", "acct-a"));
    let t = now();
    stub_usage_by_account(
        &env,
        vec![(
            "acct-a",
            StubResponse::json(
                200,
                &usage_body(
                    15.0,
                    t + 2 * 3600 + 30 * 60 + 30,
                    5.0,
                    t + 3 * 86400 + 4 * 3600 + 30,
                ),
            ),
        )],
    );

    let out = env.run(["codex", "list"]);

    out.assert_code(0);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines[1], "  1: a@example.com [plus] (active)");
    assert!(
        lines[2].starts_with("     ├ 5h:  15%   resets "),
        "{}",
        out.stdout
    );
    assert!(lines[2].ends_with("  in 2h 30m"), "{}", out.stdout);
    assert!(
        lines[3].starts_with("     └ 7d:   5%   resets "),
        "{}",
        out.stdout
    );
    assert!(lines[3].contains("  in 3d 4h"), "{}", out.stdout);
}

/// Account 1 live with a newer token generation than its home; account 2 only in its home.
fn two_accounts_with_a_live(env: &TestEnv) -> (CodexAuth, CodexAuth) {
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b").plan("team"));
    let live = CodexAuth::new("a@example.com", "acct-a").tag("refreshed");
    env.seed_codex_login(&live);
    (live, CodexAuth::new("b@example.com", "acct-b").plan("team"))
}

fn ok_body() -> StubResponse {
    let t = now();
    StubResponse::json(200, &usage_body(10.0, t + 3600, 20.0, t + 86400))
}

#[test]
fn requests_carry_the_accounts_bearer_and_chatgpt_account_id() {
    let env = TestEnv::new();
    let (live, b) = two_accounts_with_a_live(&env);
    stub_usage_by_account(&env, vec![("acct-a", ok_body()), ("acct-b", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);

    let mut seen: Vec<(String, String)> = env
        .http()
        .requests()
        .iter()
        .filter(|r| r.path() == USAGE_PATH)
        .map(|r| {
            (
                r.header("ChatGPT-Account-Id")
                    .unwrap_or_default()
                    .to_string(),
                r.header("Authorization").unwrap_or_default().to_string(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            (
                "acct-a".to_string(),
                format!("Bearer {}", live.access_token())
            ),
            ("acct-b".to_string(), format!("Bearer {}", b.access_token())),
        ]
    );
}

#[test]
fn list_json_carries_each_window() {
    let env = TestEnv::new();
    two_accounts_with_a_live(&env);
    let t = now();
    stub_usage_by_account(
        &env,
        vec![(
            "acct-a",
            StubResponse::json(200, &usage_body(15.0, t + 3630, 5.0, 4_102_444_800)),
        )],
    );

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    let payload = out.json();
    let a = &payload["accounts"][0];
    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 15.0);
    assert_eq!(a["usage"]["fiveHour"]["countdown"], "1h 0m");
    assert_eq!(a["usage"]["sevenDay"]["pct"], 5.0);
    assert_eq!(
        a["usage"]["sevenDay"]["resetsAt"],
        "2100-01-01T00:00:00+00:00"
    );
    assert!(a["usageFetchedAt"].as_str().unwrap().ends_with('Z'));
    let b = &payload["accounts"][1];
    assert_eq!(b["usageStatus"], "unavailable");
    assert_eq!(b["usage"], Value::Null);
}

#[test]
fn missing_or_unknown_fields_read_as_unknown_windows() {
    let env = TestEnv::new();
    two_accounts_with_a_live(&env);
    stub_usage_by_account(
        &env,
        vec![
            (
                "acct-a",
                StubResponse::json(
                    200,
                    r#"{"rate_limit": {"primary_window": {"used_percent": "lots", "reset_at": "soon"},
                                       "secondary_window": {"used_percent": 30, "reset_at": "soon"},
                                       "tertiary_window": {}}, "new_field": [1]}"#,
                ),
            ),
            ("acct-b", StubResponse::json(200, r#"{"rate_limit": null}"#)),
        ],
    );

    let out = env.run(["codex", "list"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Codex accounts:\n\
         \x20 1: a@example.com [plus] (active)\n\
         \x20    ├ 5h: unknown\n\
         \x20    └ 7d:  30%\n\
         \n\
         \x20 2: b@example.com [team]\n\
         \x20    ├ 5h: unknown\n\
         \x20    └ 7d: unknown\n"
    );

    let payload = env.run(["codex", "list", "--json"]).json();
    assert_eq!(
        payload["accounts"][0]["usage"],
        json!({"fiveHour": null, "sevenDay": {"pct": 30.0}})
    );
    assert_eq!(
        payload["accounts"][1]["usage"],
        json!({"fiveHour": null, "sevenDay": null})
    );
}

#[test]
fn a_body_that_is_not_json_is_a_bad_response_not_a_crash() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    stub_usage_by_account(&env, vec![("acct-a", StubResponse::json(200, "<html>"))]);

    let out = env.run(["codex", "list"]);

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("     usage unavailable (bad-response)\n"),
        "{}",
        out.stdout
    );
}

#[test]
fn the_cache_serves_within_its_ttl_and_refetches_once_stale() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    stub_usage_by_account(&env, vec![("acct-a", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);
    let out = env.run(["codex", "list"]);
    out.assert_code(0);
    assert_eq!(usage_requests(&env), 1);
    assert!(out.stdout.contains("├ 5h:  10%"), "{}", out.stdout);

    let mut cache = env.read_json(cache_path(&env));
    cache["accounts"]["1"]["fetchedAt"] = json!(now() - 600);
    env.write_json(cache_path(&env), &cache);
    env.run(["codex", "list"]).assert_code(0);
    assert_eq!(usage_requests(&env), 2);
}

#[test]
fn a_failure_backs_off_and_the_next_list_sends_nothing() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    stub_usage_by_account(&env, vec![("acct-a", StubResponse::json(503, "{}"))]);

    let out = env.run(["codex", "list"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("usage unavailable (http-503)"),
        "{}",
        out.stdout
    );
    let row = cache_row(&env, 1);
    let backoff = row["backoffUntil"].as_f64().unwrap() - row["lastAttemptAt"].as_f64().unwrap();
    assert_eq!(backoff, 30.0);

    env.run(["codex", "list"]).assert_code(0);
    assert_eq!(usage_requests(&env), 1);
}

#[test]
fn a_429_honours_retry_after_and_is_remembered() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    stub_usage_by_account(
        &env,
        vec![(
            "acct-a",
            StubResponse::json(429, "{}").header("Retry-After", "120"),
        )],
    );

    env.run(["codex", "list"]).assert_code(0);

    let row = cache_row(&env, 1);
    assert_eq!(row["lastError"], "http-429");
    let attempt = row["lastAttemptAt"].as_f64().unwrap();
    assert_eq!(row["backoffUntil"].as_f64().unwrap() - attempt, 120.0);
    assert_eq!(row["last429At"].as_f64().unwrap(), attempt);
    env.run(["codex", "list"]).assert_code(0);
    assert_eq!(usage_requests(&env), 1);
}

#[test]
fn the_codex_cache_is_its_own_and_the_claude_cache_is_untouched() {
    let env = TestEnv::new();
    env.seed_codex_account(1, &CodexAuth::new("a@example.com", "acct-a"));
    stub_usage_by_account(&env, vec![("acct-a", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);

    assert_eq!(cache_row(&env, 1)["organizationUuid"], "acct-a");
    assert!(!env.store_dir().join("cache/usage.json").exists());
}

#[test]
fn list_syncs_the_live_auth_into_the_active_accounts_home() {
    let env = TestEnv::new();
    let (live, b) = two_accounts_with_a_live(&env);
    stub_usage_by_account(&env, vec![("acct-a", ok_body()), ("acct-b", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);

    assert_eq!(env.codex_managed_auth(1), Some(live.render()));
    assert_eq!(env.codex_managed_auth(2), Some(b.render()));
}

#[test]
fn an_unregistered_live_login_is_not_copied_anywhere() {
    let env = TestEnv::new();
    let a = CodexAuth::new("a@example.com", "acct-a");
    env.seed_codex_account(1, &a);
    env.seed_codex_login(&CodexAuth::new("z@example.com", "acct-z"));
    stub_usage_by_account(&env, vec![("acct-a", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);

    assert_eq!(env.codex_managed_auth(1), Some(a.render()));
    assert!(!env.codex_managed_home(2).exists());
}

const APP_SERVER_ARGV: [&str; 5] = ["-s", "read-only", "-a", "never", "app-server"];

fn rate_limits_result(five_pct: f64, week_pct: f64) -> Value {
    json!({
        "rateLimits": {
            "limitId": "codex",
            "primary": { "usedPercent": five_pct, "windowDurationMins": 300, "resetsAt": now() + 3630 },
            "secondary": { "usedPercent": week_pct, "windowDurationMins": 10080, "resetsAt": 4_102_444_800i64 },
            "credits": { "hasCredits": false, "unlimited": false, "balance": "0" },
        },
        "rateLimitsByLimitId": {},
    })
}

/// Account 1 expired in its home; account 2 live and valid. Returns account 1 as Codex would refresh it.
fn an_expired_account_beside_a_live_one(env: &TestEnv) -> CodexAuth {
    let expired = CodexAuth::new("a@example.com", "acct-a").access_expires_at(now() - 60);
    env.seed_codex_account(1, &expired);
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    env.seed_codex_login(&CodexAuth::new("b@example.com", "acct-b"));
    stub_usage_by_account(env, vec![("acct-b", ok_body())]);
    CodexAuth::new("a@example.com", "acct-a").tag("refreshed")
}

fn script_refreshing_app_server(env: &TestEnv, refreshed: &CodexAuth) {
    env.codex().app_server(json!({
        "auth": refreshed.render(),
        "results": { "account/rateLimits/read": rate_limits_result(42.0, 7.0) },
    }));
}

fn wham_accounts(env: &TestEnv) -> Vec<String> {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == USAGE_PATH)
        .map(|r| {
            r.header("ChatGPT-Account-Id")
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn make_cache_stale(env: &TestEnv) {
    let mut cache = env.read_json(cache_path(env));
    for row in cache["accounts"].as_object_mut().unwrap().values_mut() {
        row["fetchedAt"] = json!(now() - 600);
        row["backoffUntil"] = Value::Null;
    }
    env.write_json(cache_path(env), &cache);
}

#[test]
fn an_expired_accounts_usage_comes_from_codex_app_server_on_a_copy_of_its_login() {
    let env = TestEnv::new();
    let refreshed = an_expired_account_beside_a_live_one(&env);
    script_refreshing_app_server(&env, &refreshed);

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    let a = &out.json()["accounts"][0];
    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 42.0);
    assert_eq!(a["usage"]["fiveHour"]["countdown"], "1h 0m");
    assert_eq!(a["usage"]["sevenDay"]["pct"], 7.0);
    assert_eq!(
        a["usage"]["sevenDay"]["resetsAt"],
        "2100-01-01T00:00:00+00:00"
    );
    let calls = env.codex().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].argv, APP_SERVER_ARGV);
    let staging = std::path::PathBuf::from(calls[0].codex_home.as_deref().unwrap());
    assert!(calls[0].codex_home_existed);
    assert_ne!(staging, env.codex_managed_home(1));
    assert_ne!(staging, env.codex_home());
    assert!(!staging.exists(), "{staging:?} was left behind");
    assert_eq!(env.codex_managed_auth(1), Some(refreshed.render()));
    let methods: Vec<Value> = env
        .codex()
        .app_server_requests()
        .iter()
        .map(|r| r["method"].clone())
        .collect();
    assert_eq!(
        methods,
        [
            json!("initialize"),
            json!("initialized"),
            json!("account/rateLimits/read")
        ]
    );
    assert_eq!(wham_accounts(&env), ["acct-b"]);
}

#[test]
fn the_login_app_server_refreshed_stays_in_the_home_and_the_next_fetch_is_http() {
    let env = TestEnv::new();
    let refreshed = an_expired_account_beside_a_live_one(&env);
    script_refreshing_app_server(&env, &refreshed);
    stub_usage_by_account(&env, vec![("acct-a", ok_body()), ("acct-b", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);
    assert_eq!(env.codex_managed_auth(1), Some(refreshed.render()));
    make_cache_stale(&env);
    let out = env.run(["codex", "list"]);

    out.assert_code(0);
    assert_eq!(env.codex().calls().len(), 1);
    let bearer: Vec<String> = env
        .http()
        .requests()
        .iter()
        .filter(|r| r.header("ChatGPT-Account-Id") == Some("acct-a"))
        .map(|r| r.header("Authorization").unwrap_or_default().to_string())
        .collect();
    assert_eq!(bearer, [format!("Bearer {}", refreshed.access_token())]);
}

#[test]
fn a_login_rewritten_during_the_pass_is_kept_and_the_refresh_discarded() {
    let env = TestEnv::new();
    let refreshed = an_expired_account_beside_a_live_one(&env);
    let relogin = CodexAuth::new("a@example.com", "acct-a")
        .tag("relogin")
        .render();
    env.codex().app_server(json!({
        "auth": refreshed.render(),
        "results": { "account/rateLimits/read": rate_limits_result(42.0, 7.0) },
        "meanwhile": [{ "path": env.codex_managed_home(1).join("auth.json"), "text": relogin }],
    }));

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    assert_eq!(out.json()["accounts"][0]["usage"]["fiveHour"]["pct"], 42.0);
    assert_eq!(env.codex_managed_auth(1), Some(relogin));
}

#[test]
fn a_refresh_that_is_not_a_login_is_never_committed() {
    let env = TestEnv::new();
    an_expired_account_beside_a_live_one(&env);
    let expired = env.codex_managed_auth(1);
    env.codex().app_server(json!({
        "auth": "{\"tokens\": {\"access_tok",
        "results": { "account/rateLimits/read": rate_limits_result(42.0, 7.0) },
    }));

    env.run(["codex", "list"]).assert_code(0);

    assert_eq!(env.codex_managed_auth(1), expired);
}

/// Account 1 is live with an expired token; account 2 is valid.
fn an_expired_live_account(env: &TestEnv) -> (String, CodexAuth) {
    let expired = CodexAuth::new("a@example.com", "acct-a").access_expires_at(now() - 60);
    env.seed_codex_account(1, &expired);
    let live = env.seed_codex_login(&expired);
    env.seed_codex_account(2, &CodexAuth::new("b@example.com", "acct-b"));
    stub_usage_by_account(env, vec![("acct-b", ok_body())]);
    (
        live,
        CodexAuth::new("a@example.com", "acct-a").tag("refreshed"),
    )
}

#[test]
fn the_active_accounts_refresh_runs_on_a_copy_and_lands_in_the_live_login() {
    let env = TestEnv::new();
    let (_, refreshed) = an_expired_live_account(&env);
    script_refreshing_app_server(&env, &refreshed);

    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    assert_eq!(out.json()["accounts"][0]["usage"]["fiveHour"]["pct"], 42.0);
    let calls = env.codex().calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_ne!(
        calls[0].codex_home.as_deref(),
        Some(env.codex_home().to_str().unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(env.live_codex_auth_path()).unwrap(),
        refreshed.render()
    );
    let mode = std::fs::metadata(env.live_codex_auth_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_switch_during_the_active_accounts_pass_sends_the_refresh_to_its_home() {
    let env = TestEnv::new();
    let (expired, refreshed) = an_expired_live_account(&env);
    let b = env.codex_managed_auth(2).unwrap();
    env.codex().app_server(json!({
        "auth": refreshed.render(),
        "results": { "account/rateLimits/read": rate_limits_result(42.0, 7.0) },
        "meanwhile": [
            { "path": env.codex_managed_home(1).join("auth.json"), "text": expired },
            { "path": env.live_codex_auth_path(), "text": b },
        ],
    }));

    env.run(["codex", "list"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(env.live_codex_auth_path()).unwrap(),
        b
    );
    assert_eq!(env.codex_managed_auth(1), Some(refreshed.render()));
}

#[test]
fn an_account_within_its_token_lifetime_never_starts_app_server() {
    let env = TestEnv::new();
    two_accounts_with_a_live(&env);
    stub_usage_by_account(&env, vec![("acct-a", ok_body()), ("acct-b", ok_body())]);

    env.run(["codex", "list"]).assert_code(0);

    assert!(env.codex().calls().is_empty(), "{:?}", env.codex().calls());
}

/// ccswap leaves the refresh to Codex: it calls no token endpoint itself.
#[test]
fn ccswap_never_refreshes_a_codex_token() {
    let env = TestEnv::new();
    let refreshed = an_expired_account_beside_a_live_one(&env);
    script_refreshing_app_server(&env, &refreshed);

    env.run(["codex", "list"]).assert_code(0);

    let paths: Vec<String> = env
        .http()
        .requests()
        .iter()
        .map(|r| r.path().to_string())
        .collect();
    assert_eq!(paths, [USAGE_PATH]);
    let calls = env.codex().calls();
    assert!(calls.iter().all(|c| c.argv == APP_SERVER_ARGV), "{calls:?}");
}

const REFRESH_FAILED: &str = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";

fn script_dead_login(env: &TestEnv) {
    env.codex().app_server(json!({
        "errors": { "account/rateLimits/read": { "code": -32603, "message": REFRESH_FAILED } },
    }));
}

#[test]
fn a_login_codex_cannot_refresh_reads_relogin_needed_and_is_not_retried() {
    let env = TestEnv::new();
    an_expired_account_beside_a_live_one(&env);
    script_dead_login(&env);

    let out = env.run(["codex", "list"]);

    out.assert_code(0);
    assert!(
        out.stdout.contains(
            "  1: a@example.com [plus]\n     re-login needed — Codex could not refresh its token; run: ccswap codex login\n"
        ),
        "{}",
        out.stdout
    );
    make_cache_stale(&env);
    let out = env.run(["codex", "list", "--json"]);
    out.assert_code(0);
    assert_eq!(out.json()["accounts"][0]["usageStatus"], "relogin_required");
    assert_eq!(env.codex().calls().len(), 1, "{:?}", env.codex().calls());
}

fn alive(pid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn an_app_server_that_never_answers_is_cut_off_and_leaves_no_process_behind() {
    let env = TestEnv::new();
    an_expired_account_beside_a_live_one(&env);
    env.codex().app_server(json!({ "hang": true }));

    let started = std::time::Instant::now();
    let out = env
        .cmd(["codex", "list"])
        .env("CCSWAP_TEST_CODEX_APP_SERVER_TIMEOUT_MS", "500")
        .run();

    out.assert_code(0);
    assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    assert!(
        out.stdout.contains(
            "  1: a@example.com [plus]\n     token expired — Codex could not refresh it this pass; retries automatically\n"
        ),
        "{}",
        out.stdout
    );
    let pids = env.codex().app_server_pids();
    assert_eq!(pids.len(), 2, "server and the child it spawned: {pids:?}");
    for pid in pids {
        assert!(!alive(pid), "pid {pid} survived");
    }
}

#[test]
fn a_new_login_lifts_relogin_needed() {
    let env = TestEnv::new();
    let refreshed = an_expired_account_beside_a_live_one(&env);
    script_dead_login(&env);
    env.run(["codex", "list"]).assert_code(0);

    env.seed_codex_account(1, &refreshed);
    stub_usage_by_account(&env, vec![("acct-a", ok_body()), ("acct-b", ok_body())]);
    let out = env.run(["codex", "list", "--json"]);

    out.assert_code(0);
    assert_eq!(out.json()["accounts"][0]["usageStatus"], "ok");
    assert_eq!(
        wham_accounts(&env).last().map(String::as_str),
        Some("acct-a")
    );
}
