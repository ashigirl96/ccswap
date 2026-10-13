//! Ported from upstream test_switcher.py::TestListAccountsUsage
//! (test_list_shows_usage, test_list_shows_alias_before_email,
//! test_list_shows_usage_null_reset, test_list_no_credentials,
//! test_list_uses_cached_usage, test_list_refetches_stale_entries,
//! test_on_demand_pass_persists_poll_plans,
//! test_on_demand_pass_respects_poll_plans),
//! TestDisableEnableAccount::test_list_shows_disabled_marker,
//! test_json_output.py::TestListJson (test_empty_list_no_prompt,
//! test_list_payload, test_list_payload_includes_alias,
//! test_usage_status_no_credentials_and_unavailable,
//! test_stale_usage_is_decision_gated_in_json), TestStatusJson
//! (test_status_managed, test_status_managed_includes_display_grade_last_good),
//! test_oauth.py::TestClassifyUsageError and TestFetchUsage (request headers).
//! Seeded reset times stay inside the pace suppression window to keep pace
//! markers out of these listings.

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, Impl, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const A: &str = "a@example.com";
const B: &str = "b@example.com";

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn oauth(token: &str) -> String {
    json!({"claudeAiOauth": {"accessToken": token, "refreshToken": format!("rt-{token}"),
                             "expiresAt": (now() as i64 + 86400) * 1000}})
    .to_string()
}

fn two_accounts(env: &TestEnv) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": A, "uuid": "u1", "organizationUuid": "", "organizationName": "",
                  "alias": "dev", "added": "2024-01-01T00:00:00Z"},
            "2": {"email": B, "uuid": "u2", "organizationUuid": "org-2",
                  "organizationName": "Acme", "added": "2024-01-02T00:00:00Z"}
        }
    }));
    env.seed_login(A, "", "");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &oauth("tok-a"));
    env.security()
        .set_item("claude-swap", &format!("account-2-{B}"), &oauth("tok-b"));
}

fn identities() -> [(&'static str, &'static str, &'static str); 2] {
    [("1", A, ""), ("2", B, "org-2")]
}

fn stub_usage(env: &TestEnv, by_token: Vec<(&'static str, StubResponse)>) {
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            let auth = req.header("Authorization").unwrap_or_default().to_string();
            by_token
                .iter()
                .find(|(t, _)| auth == format!("Bearer {t}"))
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| StubResponse::json(500, "{}"))
        });
}

fn usage_requests(env: &TestEnv) -> usize {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/oauth/usage")
        .count()
}

fn seed_cache(env: &TestEnv, rows: &[(&str, Value)]) {
    let mut accounts = serde_json::Map::new();
    for (num, extra) in rows {
        let (_, email, org) = identities().into_iter().find(|i| i.0 == *num).unwrap();
        let mut row = json!({"email": email, "organizationUuid": org});
        for (k, v) in extra.as_object().unwrap() {
            row[k] = v.clone();
        }
        accounts.insert(num.to_string(), row);
    }
    env.write_json(
        ".claude-swap-backup/cache/usage.json",
        &json!({"schemaVersion": 2, "accounts": accounts}),
    );
}

fn cache_row(env: &TestEnv, num: &str) -> Value {
    env.read_json(".claude-swap-backup/cache/usage.json")["accounts"][num].clone()
}

#[test]
fn list_shows_usage_markers_alias_and_org() {
    let env = TestEnv::new();
    two_accounts(&env);
    stub_usage(
        &env,
        vec![
            (
                "tok-a",
                StubResponse::json(
                    200,
                    r#"{"five_hour": {"utilization": 10.0, "resets_at": null},
                        "seven_day": {"utilization": 100.0}}"#,
                ),
            ),
            (
                "tok-b",
                StubResponse::json(200, r#"{"five_hour": {"utilization": 0}}"#),
            ),
        ],
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Accounts:\n\
         \x20 1: dev (a@example.com) [personal] (active)\n\
         \x20    ├ 5h:  10%\n\
         \x20    └ 7d: 100%\n\
         \n\
         \x20 2: b@example.com [Acme]\n\
         \x20    └ 5h:   0%\n"
    );
}

#[test]
fn usage_request_carries_bearer_beta_and_user_agent() {
    let env = TestEnv::new();
    two_accounts(&env);
    stub_usage(&env, vec![("tok-a", StubResponse::json(200, "{}"))]);

    env.run(["ls"]).assert_code(0);
    let reqs = env.http().requests();
    let req = reqs
        .iter()
        .find(|r| r.header("Authorization") == Some("Bearer tok-a"))
        .expect("active account fetched");
    assert_eq!(req.method, "GET");
    assert_eq!(req.header("anthropic-beta"), Some("oauth-2025-04-20"));
    assert_eq!(req.header("User-Agent"), Some("claude-swap/1.0"));
}

#[test]
fn reset_times_render_as_clock_and_countdown() {
    let env = TestEnv::new();
    two_accounts(&env);
    let five = now() as i64 + 2 * 3600 + 30 * 60 + 30;
    let body = json!({
        "five_hour": {"utilization": 42, "resets_at": iso(five)},
        "extra_usage": {"is_enabled": true, "used_credits": 123456, "monthly_limit": 500000,
                        "utilization": 24.7},
        "limits": [{"scope": {"model": {"display_name": "Fable"}}, "percent": 100,
                    "resets_at": iso(five)}],
    });
    stub_usage(
        &env,
        vec![("tok-a", StubResponse::json(200, &body.to_string()))],
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines[2], "     ├ $$:     25%   $1,234.56 / $5,000.00");
    assert!(
        lines[3].starts_with("     ├ 5h:     42%   resets "),
        "{}",
        lines[3]
    );
    assert!(lines[3].ends_with("  in 2h 30m"), "{}", lines[3]);
    assert!(
        lines[4].starts_with("     └ Fable: 100%   resets "),
        "{}",
        lines[4]
    );
    assert!(lines[4].ends_with("  in 2h 30m  (!)"), "{}", lines[4]);
}

fn iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[test]
fn list_json_payload_shape() {
    let env = TestEnv::new();
    two_accounts(&env);
    stub_usage(
        &env,
        vec![(
            "tok-a",
            StubResponse::json(
                200,
                r#"{"five_hour": {"utilization": 10.0, "resets_at": "2026-01-01T00:00:00Z"}}"#,
            ),
        )],
    );

    let out = env.run(["list", "--json"]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["schemaVersion"], 1);
    assert_eq!(payload["activeAccountNumber"], 1);
    let a = &payload["accounts"][0];
    let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "number",
            "email",
            "organizationName",
            "organizationUuid",
            "isOrganization",
            "active",
            "usageStatus",
            "usage",
            "alias",
            "usageFetchedAt",
            "usageAgeSeconds"
        ]
    );
    assert_eq!(a["active"], true);
    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["alias"], "dev");
    assert_eq!(
        a["usage"],
        json!({"fiveHour": {"pct": 10.0, "resetsAt": "2026-01-01T00:00:00Z",
                            "countdown": "0m", "clock": a["usage"]["fiveHour"]["clock"]}})
    );
    assert!(a["usageFetchedAt"].as_str().unwrap().ends_with('Z'));
    assert!(a["usageAgeSeconds"].as_f64().unwrap() < 60.0);

    let b = &payload["accounts"][1];
    assert_eq!(
        *b,
        json!({"number": 2, "email": B, "organizationName": "Acme", "organizationUuid": "org-2",
               "isOrganization": true, "active": false, "usageStatus": "unavailable",
               "usage": null})
    );
}

#[test]
fn json_without_a_store_is_empty_and_never_prompts() {
    let env = TestEnv::new();
    env.seed_login(A, "", "");
    let out = env.run(["list", "--json"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "{\n  \"schemaVersion\": 1,\n  \"activeAccountNumber\": null,\n  \"accounts\": []\n}\n"
    );
}

#[test]
fn missing_credentials_and_disabled_slots() {
    let env = TestEnv::new();
    two_accounts(&env);
    let mut seq = env.read_json(".claude-swap-backup/sequence.json");
    seq["accounts"]["2"]["disabled"] = true.into();
    env.seed_sequence(seq);
    env.security()
        .set_item("claude-swap", &format!("account-2-{B}"), "");
    stub_usage(&env, vec![("tok-a", StubResponse::json(200, "{}"))]);

    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .ends_with("  2: b@example.com [Acme] (disabled)\n     no credentials\n"),
        "{}",
        out.stdout
    );
    let payload = env.run(["list", "--json"]).json();
    assert_eq!(payload["accounts"][1]["usageStatus"], "no_credentials");
    assert_eq!(payload["accounts"][1]["disabled"], true);
    assert_eq!(payload["accounts"][0]["usageStatus"], "unavailable");
}

#[test]
fn fresh_cache_is_served_without_a_request_and_success_persists_a_plan() {
    let env = TestEnv::new();
    two_accounts(&env);
    stub_usage(
        &env,
        vec![
            (
                "tok-a",
                StubResponse::json(200, r#"{"five_hour": {"utilization": 25}}"#),
            ),
            (
                "tok-b",
                StubResponse::json(200, r#"{"five_hour": {"utilization": 80}}"#),
            ),
        ],
    );

    env.run(["list"]).assert_code(0);
    assert_eq!(usage_requests(&env), 2);
    for (num, _, _) in identities() {
        let row = cache_row(&env, num);
        assert!(row["nextPollAt"].as_f64().unwrap() > now(), "{row}");
        assert!(row["pollIntervalS"].as_f64().unwrap() >= 180.0, "{row}");
        assert_eq!(row["claimId"], Value::Null);
        assert_eq!(row["claimUntil"], 0.0);
    }

    let out = env.run(["list"]);
    out.assert_code(0);
    assert_eq!(usage_requests(&env), 2, "served from cache");
    assert!(out.stdout.contains("5h:  25%"), "{}", out.stdout);
    assert!(out.stdout.contains("5h:  80%"), "{}", out.stdout);
}

#[test]
fn stale_rows_refetch_but_a_future_plan_is_respected() {
    let env = TestEnv::new();
    two_accounts(&env);
    let t = now();
    seed_cache(
        &env,
        &[
            (
                "1",
                json!({"lastGood": {"five_hour": {"pct": 25}}, "fetchedAt": t - 400.0}),
            ),
            (
                "2",
                json!({"lastGood": {"five_hour": {"pct": 33}}, "fetchedAt": t - 400.0,
                       "nextPollAt": t + 600.0, "pollIntervalS": 600.0}),
            ),
        ],
    );
    stub_usage(
        &env,
        vec![
            (
                "tok-a",
                StubResponse::json(200, r#"{"five_hour": {"utilization": 10}}"#),
            ),
            (
                "tok-b",
                StubResponse::json(200, r#"{"five_hour": {"utilization": 90}}"#),
            ),
        ],
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    assert_eq!(usage_requests(&env), 1);
    assert!(out.stdout.contains("5h:  10%"), "{}", out.stdout);
    assert!(!out.stdout.contains("25%"), "{}", out.stdout);
    assert!(out.stdout.contains("5h:  33% · 6m ago"), "{}", out.stdout);
}

#[test]
fn failed_fetch_serves_last_good_with_its_age() {
    let env = TestEnv::new();
    two_accounts(&env);
    let t = now();
    seed_cache(
        &env,
        &[(
            "1",
            json!({"lastGood": {"five_hour": {"pct": 25.0}}, "fetchedAt": t - 400.0}),
        )],
    );
    stub_usage(&env, vec![("tok-a", StubResponse::json(503, "{}"))]);

    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout.contains("     └ 5h:  25% · 6m ago\n"),
        "{}",
        out.stdout
    );
    let row = cache_row(&env, "1");
    assert_eq!(row["lastError"], "http-503");
    assert_eq!(row["consecutiveFailures"], 1);
    assert_eq!(row["fetchedAt"].as_f64().unwrap(), t - 400.0);

    let a = env.run(["list", "--json"]).json()["accounts"][0].clone();
    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 25.0);
    assert!(a["usageAgeSeconds"].as_f64().unwrap() >= 400.0);
    assert!(a.get("lastGoodUsage").is_none());
}

#[test]
fn last_good_past_the_trust_ceiling_is_not_decision_grade() {
    let env = TestEnv::new();
    two_accounts(&env);
    let t = now();
    seed_cache(
        &env,
        &[(
            "1",
            json!({"lastGood": {"five_hour": {"pct": 25.0}}, "fetchedAt": t - 4000.0}),
        )],
    );
    stub_usage(&env, vec![("tok-a", StubResponse::json(504, "{}"))]);

    let a = env.run(["list", "--json"]).json()["accounts"][0].clone();
    assert_eq!(a["usageStatus"], "unavailable");
    assert_eq!(a["usage"], Value::Null);
    assert!(a.get("usageFetchedAt").is_none());
    assert_eq!(a["lastGoodUsage"]["fiveHour"]["pct"], 25.0);
    assert!(a["lastGoodAgeSeconds"].as_f64().unwrap() >= 4000.0);
    assert!(a["lastGoodFetchedAt"].as_str().unwrap().ends_with('Z'));

    let out = env.run(["list"]);
    assert!(
        out.stdout.contains("     └ 5h:  25% · 1h ago\n"),
        "{}",
        out.stdout
    );
}

fn first_error(env: &TestEnv, response: StubResponse) -> (Value, String) {
    two_accounts(env);
    stub_usage(env, vec![("tok-a", response)]);
    let out = env.run(["list"]);
    out.assert_code(0);
    (cache_row(env, "1"), out.stdout)
}

#[test]
fn http_errors_are_classified_and_shown_without_data() {
    let env = TestEnv::new();
    let (row, stdout) = first_error(&env, StubResponse::json(503, "{}"));
    assert_eq!(row["lastError"], "http-503");
    assert!(
        stdout.contains("(active)\n     usage unavailable (http-503)\n"),
        "{stdout}"
    );
    let backoff = row["backoffUntil"].as_f64().unwrap() - row["lastAttemptAt"].as_f64().unwrap();
    assert_eq!(backoff, 30.0);
    assert!(row.get("last429At").is_none());
}

#[test]
fn retry_after_seconds_set_the_backoff_and_mark_the_429() {
    let env = TestEnv::new();
    let (row, _) = first_error(
        &env,
        StubResponse::json(429, "{}").header("Retry-After", "120"),
    );
    assert_eq!(row["lastError"], "http-429");
    let attempt = row["lastAttemptAt"].as_f64().unwrap();
    assert_eq!(row["backoffUntil"].as_f64().unwrap() - attempt, 120.0);
    assert_eq!(row["last429At"].as_f64().unwrap(), attempt);
}

#[test]
fn retry_after_dates_are_ignored() {
    let env = TestEnv::new();
    let (row, _) = first_error(
        &env,
        StubResponse::json(503, "{}").header("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT"),
    );
    let backoff = row["backoffUntil"].as_f64().unwrap() - row["lastAttemptAt"].as_f64().unwrap();
    assert_eq!(backoff, 30.0);
}

#[test]
fn unparseable_body_is_a_bad_response() {
    let env = TestEnv::new();
    let (row, _) = first_error(&env, StubResponse::json(200, "<html>"));
    assert_eq!(row["lastError"], "bad-response");
}

#[test]
fn backoff_suppresses_the_next_request() {
    let env = TestEnv::new();
    first_error(&env, StubResponse::json(503, "{}"));
    let before = usage_requests(&env);
    env.run(["list"]).assert_code(0);
    assert_eq!(usage_requests(&env), before);
}

#[test]
fn unreachable_endpoint_is_a_network_error() {
    let env = TestEnv::new();
    two_accounts(&env);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    env.cmd(["list"])
        .env("CCSWAP_API_BASE_URL", &url)
        .run()
        .assert_code(0);
    assert_eq!(cache_row(&env, "1")["lastError"], "network");
}

#[test]
fn slow_endpoint_is_a_timeout() {
    let env = TestEnv::new();
    two_accounts(&env);
    env.http().respond_with("GET", "/api/oauth/usage", |_| {
        std::thread::sleep(std::time::Duration::from_secs(7));
        StubResponse::json(200, "{}")
    });
    env.run(["list"]).assert_code(0);
    assert_eq!(cache_row(&env, "1")["lastError"], "timeout");
}

#[test]
fn status_shows_and_reports_the_active_accounts_usage() {
    let env = TestEnv::new();
    two_accounts(&env);
    stub_usage(
        &env,
        vec![(
            "tok-a",
            StubResponse::json(
                200,
                r#"{"five_hour": {"utilization": 25.0}, "seven_day": {"utilization": 7}}"#,
            ),
        )],
    );

    let out = env.run(["status"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Status: Account-1 (a@example.com [personal])\n  Total managed accounts: 2\n  ├ 5h:  25%\n  └ 7d:   7%\n"
    );
    assert_eq!(
        usage_requests(&env),
        1,
        "status fetches only the active slot"
    );

    let active = env.run(["status", "--json"]).json()["active"].clone();
    assert_eq!(active["usageStatus"], "ok");
    assert_eq!(
        active["usage"],
        json!({"fiveHour": {"pct": 25.0}, "sevenDay": {"pct": 7}})
    );
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
            "alias",
            "usageFetchedAt",
            "usageAgeSeconds"
        ]
    );
    assert_eq!(usage_requests(&env), 1);
}

#[test]
fn caches_round_trip_between_ccswap_and_cswap() {
    if !support::is_upstream() {
        return;
    }
    for (writer, reader) in [
        (Impl::Ccswap, Impl::Upstream),
        (Impl::Upstream, Impl::Ccswap),
    ] {
        let env = TestEnv::new();
        two_accounts(&env);
        stub_usage(
            &env,
            vec![
                (
                    "tok-a",
                    StubResponse::json(200, r#"{"five_hour": {"utilization": 25}}"#),
                ),
                (
                    "tok-b",
                    StubResponse::json(200, r#"{"five_hour": {"utilization": 80}}"#),
                ),
            ],
        );
        let first = env.cmd(["list"]).using(writer).run();
        first.assert_code(0);
        assert_eq!(usage_requests(&env), 2);
        let second = env.cmd(["list"]).using(reader).run();
        second.assert_code(0);
        assert_eq!(usage_requests(&env), 2, "{writer:?} -> {reader:?}");
        assert_eq!(first.stdout, second.stdout);
    }
}
