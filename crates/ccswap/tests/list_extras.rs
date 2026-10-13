//! `list` extras: weekly pace, duplicate/lockstep warnings, running
//! instances, `--token-status` and the first-run setup.
//!
//! Ported from upstream test_switcher.py::TestFormatUsageLines (pace
//! markers), test_json_output.py::TestJsonHelpers (pace fields),
//! TestDuplicateAccountDetection, TestLockstepUsageDetection,
//! TestListAccountsUsage::test_list_shows_token_status_when_requested and
//! the `_token_status_lines` cases, test_oauth.py::TestBuildTokenStatus,
//! test_process_detection.py (TestListSessions, TestListIdeInstances,
//! TestGetRunningInstances) and test_json_output.py::test_empty_list_no_prompt.

mod support;

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv, prog};

const A: &str = "a@example.com";
const B: &str = "b@example.com";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn oauth_expiring(token: &str, refresh: &str, expires_in_s: i64) -> String {
    json!({"claudeAiOauth": {"accessToken": token, "refreshToken": refresh,
                             "expiresAt": (now() + expires_in_s) * 1000}})
    .to_string()
}

fn oauth(token: &str) -> String {
    oauth_expiring(token, &format!("rt-{token}"), 86400)
}

fn two_accounts(env: &TestEnv, creds_a: &str, creds_b: &str) {
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": A, "uuid": "u1", "organizationUuid": "", "organizationName": "",
                  "added": "2024-01-01T00:00:00Z"},
            "2": {"email": B, "uuid": "u2", "organizationUuid": "org-2",
                  "organizationName": "Acme", "added": "2024-01-02T00:00:00Z"}
        }
    }));
    env.seed_login(A, "", "");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, creds_a);
    env.security()
        .set_item("claude-swap", &format!("account-2-{B}"), creds_b);
}

fn stub_usage(env: &TestEnv, by_token: Vec<(&'static str, Value)>) {
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            let auth = req.header("Authorization").unwrap_or_default().to_string();
            by_token
                .iter()
                .find(|(t, _)| auth == format!("Bearer {t}"))
                .map(|(_, body)| StubResponse::json(200, &body.to_string()))
                .unwrap_or_else(|| StubResponse::json(500, "{}"))
        });
}

fn iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let secs = epoch.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

const DAY: i64 = 86400;

#[test]
fn weekly_windows_ahead_of_pace_are_marked_and_projected() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), "");
    let resets = iso(now() + 5 * DAY);
    stub_usage(
        &env,
        vec![(
            "tok-a",
            json!({
                "five_hour": {"utilization": 90, "resets_at": iso(now() + 3600)},
                "seven_day": {"utilization": 60, "resets_at": resets},
                "limits": [
                    {"scope": {"model": {"display_name": "Fable"}}, "percent": 60, "resets_at": resets},
                    {"scope": {"model": {"display_name": "Opus"}}, "percent": 10, "resets_at": resets}
                ],
            }),
        )],
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(!lines[2].contains("pace"), "{}", lines[2]);
    assert!(lines[3].ends_with("  (ahead of pace)"), "{}", lines[3]);
    assert!(lines[4].ends_with("  (ahead of pace)"), "{}", lines[4]);
    assert!(!lines[5].contains("pace"), "{}", lines[5]);

    let payload = env.run(["list", "--json"]).json();
    let usage = &payload["accounts"][0]["usage"];
    assert!(usage["fiveHour"].get("aheadOfPace").is_none());
    let week = &usage["sevenDay"];
    assert_eq!(week["aheadOfPace"], true);
    assert_eq!(week["willLastToReset"], false);
    let expected = week["expectedPct"].as_f64().unwrap();
    assert!((28.5..=28.6).contains(&expected), "{expected}");
    assert!(
        week["projectedExhaustionAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    let opus = &usage["scoped"][1];
    assert_eq!(opus["aheadOfPace"], false);
    assert_eq!(opus["willLastToReset"], true);
    assert_eq!(
        opus.as_object().unwrap().keys().next_back().unwrap(),
        "name"
    );
}

#[test]
fn slots_holding_the_same_credential_or_identity_are_flagged() {
    let env = TestEnv::new();
    let shared = oauth("tok-a");
    two_accounts(&env, &shared, &shared);
    let mut seq = env.read_json(".claude-swap-backup/sequence.json");
    seq["sequence"] = json!([1, 2, 3]);
    seq["accounts"]["3"] = json!({"email": "c@example.com", "uuid": "u2",
        "organizationUuid": "org-2", "organizationName": "Acme", "added": "2024-01-03T00:00:00Z"});
    env.seed_sequence(seq);
    env.security()
        .set_item("claude-swap", "account-3-c@example.com", &oauth("tok-c"));
    stub_usage(&env, vec![]);

    let dup_fp = "Account-1 and Account-2 hold the same credential (b@example.com) — one slot's \
                  backup was overwritten. Log in with the missing account and re-add it: cswap add --slot N";
    let dup_id = "Account-2 and Account-3 both authenticate as c@example.com — remove or re-login one of them.";
    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout.ends_with(&format!("\n\n{dup_fp}\n{dup_id}\n")),
        "{}",
        out.stdout
    );

    let payload = env.run(["list", "--json"]).json();
    assert_eq!(payload["duplicateAccountWarnings"], json!([dup_fp, dup_id]));
    assert!(payload.get("lockstepUsageWarnings").is_none());
}

#[test]
fn empty_uuids_and_distinct_credentials_are_never_flagged() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), &oauth("tok-b"));
    let mut seq = env.read_json(".claude-swap-backup/sequence.json");
    seq["accounts"]["1"]["uuid"] = "".into();
    seq["accounts"]["2"]["uuid"] = "".into();
    seq["accounts"]["2"]["organizationUuid"] = "".into();
    env.seed_sequence(seq);
    stub_usage(&env, vec![]);

    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(!out.stdout.contains("Account-"), "{}", out.stdout);
    let payload = env.run(["list", "--json"]).json();
    assert!(payload.get("duplicateAccountWarnings").is_none());
}

#[test]
fn slots_reporting_identical_usage_and_resets_are_flagged_as_lockstep() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), &oauth("tok-b"));
    let same = json!({
        "five_hour": {"utilization": 25.0, "resets_at": iso(now() + 3600)},
        "seven_day": {"utilization": 60.0, "resets_at": iso(now() + 7 * DAY - 3600)},
    });
    stub_usage(&env, vec![("tok-a", same.clone()), ("tok-b", same)]);

    let msg = "Account-1 and Account-2 report identical usage and reset times — they may be the \
               same account (issue #117). If it persists, log in with the missing account and \
               re-add it: cswap add --slot N";
    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout.ends_with(&format!("\n\n{msg}\n")),
        "{}",
        out.stdout
    );
    let payload = env.run(["list", "--json"]).json();
    assert_eq!(payload["lockstepUsageWarnings"], json!([msg]));
    assert!(payload.get("duplicateAccountWarnings").is_none());
}

#[test]
fn usage_without_reset_times_or_with_different_resets_is_not_lockstep() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), &oauth("tok-b"));
    let idle = json!({"five_hour": {"utilization": 0.0}, "seven_day": {"utilization": 0.0}});
    let a = json!({
        "five_hour": {"utilization": 25.0, "resets_at": iso(now() + 3600)},
        "seven_day": {"utilization": 60.0, "resets_at": iso(now() + 7 * DAY - 3600)},
    });
    let mut b = a.clone();
    b["five_hour"]["resets_at"] = iso(now() + 7200).into();
    for (ua, ub) in [(idle.clone(), idle), (a, b)] {
        env.write_json(".claude-swap-backup/cache/usage.json", &json!({}));
        stub_usage(&env, vec![("tok-a", ua), ("tok-b", ub)]);
        let payload = env.run(["list", "--json"]).json();
        assert!(payload.get("lockstepUsageWarnings").is_none(), "{payload}");
    }
}

fn write_session(env: &TestEnv, name: &str, body: &Value) {
    env.write_json(env.claude_config_home().join("sessions").join(name), body);
}

#[test]
fn running_instances_group_live_sessions_and_ide_folders() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), "");
    stub_usage(&env, vec![]);
    let me = std::process::id();
    let home = env.home().display().to_string();
    let app = format!("{home}/work/app");
    let session = |entrypoint: &str| {
        json!({"pid": me, "sessionId": "s", "cwd": app, "startedAt": 0,
               "kind": "interactive", "entrypoint": entrypoint, "status": "idle"})
    };
    write_session(&env, "1.json", &session("cli"));
    write_session(&env, "2.json", &session("cli"));
    write_session(
        &env,
        "3.json",
        &json!({"pid": 999_999, "cwd": "/dead", "entrypoint": "cli"}),
    );
    write_session(&env, "4.json", &json!({"cwd": "/no-pid"}));
    write_session(&env, "5.json", &json!([me]));
    env.write_file(
        env.claude_config_home().join("sessions/6.json"),
        "{not json",
    );
    env.write_file(
        env.claude_config_home().join("sessions/notes.txt"),
        "ignored",
    );
    env.write_json(
        env.claude_config_home().join("ide/43117.lock"),
        &json!({"pid": me, "ideName": "Visual Studio Code",
                "workspaceFolders": [app, "/srv/other"]}),
    );
    env.write_json(
        env.claude_config_home().join("ide/43118.lock"),
        &json!({"pid": 999_999, "ideName": "Cursor", "workspaceFolders": ["/dead"]}),
    );
    env.write_json(
        env.claude_config_home().join("ide/not-a-port.lock"),
        &json!({"pid": me, "ideName": "Cursor", "workspaceFolders": ["/bad-port"]}),
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout.ends_with(
            "\n\nRunning instances:\n\
             \x20 ● CLI   ~/work/app  (2 sessions)\n\
             \x20 ● VS Code   ~/work/app  (IDE)\n\
             \x20 ● VS Code   /srv/other  (IDE)\n"
        ),
        "{}",
        out.stdout
    );
    let payload = env.run(["list", "--json"]).json();
    assert_eq!(
        payload.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["schemaVersion", "activeAccountNumber", "accounts"]
    );
}

#[test]
fn running_instances_follow_claude_config_dir_and_merge_session_and_ide() {
    let mut env = TestEnv::new();
    env.use_custom_config_dir();
    two_accounts(&env, &oauth("tok-a"), "");
    stub_usage(&env, vec![]);
    let me = std::process::id();
    write_session(
        &env,
        "1.json",
        &json!({"pid": me, "cwd": "/srv/x", "entrypoint": "claude-vscode"}),
    );
    env.write_json(
        env.claude_config_home().join("ide/1.lock"),
        &json!({"pid": me, "workspaceFolders": ["/srv/x"]}),
    );
    env.write_json(
        env.claude_config_home().join("ide/2.lock"),
        &json!({"pid": me, "ideName": "Visual Studio Code", "workspaceFolders": ["/srv/x"]}),
    );

    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(
        out.stdout.ends_with(
            "Running instances:\n\
             \x20 ● VS Code   /srv/x  (1 session, IDE)\n\
             \x20 ● Unknown IDE   /srv/x  (IDE)\n"
        ),
        "{}",
        out.stdout
    );
}

#[test]
fn no_running_instances_section_without_live_records() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), "");
    stub_usage(&env, vec![]);
    write_session(
        &env,
        "1.json",
        &json!({"pid": 999_999, "cwd": "/x", "entrypoint": "cli"}),
    );
    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(!out.stdout.contains("Running instances"), "{}", out.stdout);
}

fn token_lines(stdout: &str, slot: &str) -> Vec<String> {
    let header = format!("  {slot}: ");
    stdout
        .lines()
        .skip_while(|l| !l.starts_with(&header))
        .skip(1)
        .take_while(|l| l.starts_with("     "))
        .filter_map(|l| l.strip_prefix("     • "))
        .map(str::to_string)
        .collect()
}

fn assert_status(line: &str, prefix: &str, countdown: &str) {
    assert!(line.starts_with(prefix), "{line}");
    assert!(line.ends_with(countdown), "{line}");
}

#[test]
fn token_status_labels_each_credential_source() {
    let env = TestEnv::new();
    two_accounts(
        &env,
        &oauth_expiring("tok-a", "rt-a", 2 * 3600 + 30),
        &oauth_expiring("tok-b", "", DAY + 3 * 3600 + 30),
    );
    let mut seq = env.read_json(".claude-swap-backup/sequence.json");
    seq["sequence"] = json!([1, 2, 3]);
    seq["accounts"]["3"] = json!({"email": "key@example.com", "uuid": "",
        "organizationUuid": "", "organizationName": "", "added": "2024-01-03T00:00:00Z"});
    env.seed_sequence(seq);
    env.security().set_item(
        "claude-swap",
        "account-3-key@example.com",
        "sk-ant-api03-test",
    );
    let profile = env.store_dir().join("sessions/2-b_example.com");
    env.write_file(
        profile.join(".credentials.json"),
        &json!({"claudeAiOauth": {"accessToken": "s", "refreshToken": "rt-s",
                                  "expiresAt": (now() - 3600) * 1000}})
        .to_string(),
    );
    env.write_json(
        profile.join(".claude.json"),
        &json!({"oauthAccount": {"emailAddress": B, "organizationUuid": "org-2"}}),
    );
    stub_usage(&env, vec![]);

    let out = env.run(["list", "--token-status"]);
    out.assert_code(0);
    let active = token_lines(&out.stdout, "1");
    assert_eq!(active.len(), 1, "{}", out.stdout);
    assert_status(
        &active[0],
        "active profile: fresh, refresh token yes, expires ",
        " in 2h 0m",
    );
    let inactive = token_lines(&out.stdout, "2");
    assert_eq!(inactive.len(), 2, "{}", out.stdout);
    assert_status(
        &inactive[0],
        "session profile: expired, refresh token yes, expires ",
        " in 0m",
    );
    assert_status(
        &inactive[1],
        "stored backup: fresh, refresh token no, expires ",
        " in 1d 3h",
    );
    assert_eq!(token_lines(&out.stdout, "3"), Vec::<String>::new());

    let plain = env.run(["list"]);
    assert!(!plain.stdout.contains('•'), "{}", plain.stdout);
}

#[test]
fn token_status_ignores_a_drifted_session_profile_and_unknown_expiry() {
    let env = TestEnv::new();
    two_accounts(
        &env,
        &oauth("tok-a"),
        &json!({"claudeAiOauth": {"accessToken": "tok-b", "refreshToken": "rt-b"}}).to_string(),
    );
    let profile = env.store_dir().join("sessions/2-b_example.com");
    env.write_file(profile.join(".credentials.json"), &oauth("tok-s"));
    env.write_json(
        profile.join(".claude.json"),
        &json!({"oauthAccount": {"emailAddress": "other@example.com"}}),
    );
    stub_usage(&env, vec![]);

    let out = env.run(["list", "--token-status"]);
    out.assert_code(0);
    assert_eq!(
        token_lines(&out.stdout, "2"),
        [
            "session profile: ignored (different account)",
            "stored backup: unknown expiry, refresh token yes",
        ]
    );
}

#[test]
fn unclaimed_credentials_are_listed_sorted_in_json_only() {
    let env = TestEnv::new();
    two_accounts(&env, &oauth("tok-a"), "");
    stub_usage(&env, vec![]);
    let creds = env.store_dir().join("credentials");
    env.write_file(creds.join(".unclaimed-zz9.enc"), "x");
    env.write_json(
        creds.join(".unclaimed-manifest.json"),
        &json!({"entries": {"bb2": {"createdAt": "2024-01-01T00:00:00Z"},
                            "aa1": {"createdAt": "2024-01-01T00:00:00Z"}}}),
    );

    let payload = env.run(["list", "--json"]).json();
    assert_eq!(
        payload["unclaimedCredentials"],
        json!(["aa1", "bb2", "zz9"])
    );
    let out = env.run(["list"]);
    out.assert_code(0);
    assert!(!out.stdout.contains("zz9"), "{}", out.stdout);
}

#[test]
fn first_run_without_a_login_asks_to_log_in() {
    let env = TestEnv::new();
    let out = env.run(["list"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "No accounts are managed yet.\nNo active Claude account found. Please log in first.\n"
    );
}

#[test]
fn first_run_can_be_declined() {
    let env = TestEnv::new();
    env.seed_login(A, "", "");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &oauth("tok-a"));
    let out = env.cmd(["list"]).stdin("N\n").run();
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "No accounts are managed yet.\nNo managed accounts found. Add current account \
             ({A}) to managed list? [Y/n] Setup cancelled. You can run '{}' later.\n",
            if prog() == "cswap" {
                "cswap --add-account"
            } else {
                "ccswap --add-account"
            }
        )
    );
    assert!(!env.store_dir().join("sequence.json").exists());
}

#[test]
fn first_run_accepting_registers_the_live_login() {
    let env = TestEnv::new();
    env.seed_login(A, "", "");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &oauth("tok-a"));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &json!({"account": {"uuid": format!("uuid-{A}"), "email": A}}).to_string(),
        ),
    );
    let out = env.cmd(["list"]).stdin("\n").run();
    out.assert_code(0);
    let seq = env.read_json(".claude-swap-backup/sequence.json");
    assert_eq!(seq["sequence"], json!([1]));
    assert_eq!(seq["accounts"]["1"]["email"], A);
}
