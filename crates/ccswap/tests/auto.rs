//! Ported from upstream tests/test_cli.py::TestAutoCommand and
//! tests/test_config_cli.py::test_auto_picks_up_configured_threshold, with
//! the real engine in place of FakeEngine.

mod support;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, Output, PROG, StubResponse, TestEnv};

const EMAILS: [&str; 4] = ["", "a@example.com", "b@example.com", "c@example.com"];

fn creds(num: u32) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("at-{num}"),
        "refreshToken": format!("rt-{num}"),
        "expiresAt": 9_999_999_999_000_i64,
    }})
    .to_string()
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": EMAILS[num as usize], "accountUuid": format!("uuid-{num}")})
}

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
        let email = EMAILS[*n as usize];
        env.security()
            .set_item("claude-swap", &format!("account-{n}-{email}"), &creds(*n));
        env.write_file(
            env.store_dir()
                .join(format!("configs/.claude-config-{n}-{email}.json")),
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

fn three(env: &TestEnv) {
    seed(env, &[1, 2, 3], 1);
}

fn stub(env: &TestEnv, bodies: Vec<(u32, Value)>) {
    let bodies: Vec<(String, String)> = bodies
        .into_iter()
        .map(|(n, b)| (format!("Bearer at-{n}"), b.to_string()))
        .collect();
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            let auth = req.header("Authorization").unwrap_or_default().to_string();
            bodies
                .iter()
                .find(|(a, _)| *a == auth)
                .map(|(_, b)| StubResponse::json(200, b))
                .unwrap_or_else(|| StubResponse::json(500, "{}"))
        });
}

fn five_hour(pct: f64) -> Value {
    json!({"five_hour": {"utilization": pct}, "seven_day": {"utilization": 0.0}})
}

fn stub_five_hour(env: &TestEnv, rows: &[(u32, f64)]) {
    stub(env, rows.iter().map(|(n, p)| (*n, five_hour(*p))).collect());
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

fn reasons(events: &[Value]) -> Vec<String> {
    of_kind(events, "no-switch")
        .iter()
        .map(|e| e["reason"].as_str().unwrap().to_string())
        .collect()
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
    env.read_json(env.store_dir().join("autoswitch_state.json"))
}

fn state_path(env: &TestEnv) -> std::path::PathBuf {
    env.store_dir().join("autoswitch_state.json")
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn once_exit_code_switched() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 95.0), (2, 40.0), (3, 20.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(0);
    let events = events(&out);
    let switch = of_kind(&events, "switch");
    assert_eq!(switch.len(), 1, "{}", out.stdout);
    assert_eq!(switch[0]["trigger"], "proactive");
    assert_eq!(switch[0]["from"], json!({"number": 1, "email": EMAILS[1]}));
    assert_eq!(switch[0]["to"], json!({"number": 3, "email": EMAILS[3]}));
    assert_eq!(switch[0]["dryRun"], false);
    assert_eq!(live_token(&env), "at-3");

    let state = state(&env);
    assert_eq!(state["schemaVersion"], 1);
    assert_eq!(state["lastSwitchTo"], "3");
    assert_eq!(state["lastSwitchFrom"], 1);
    assert_eq!(state["leftHeadroom"], 5.0);
    assert_eq!(state["leftTrigger"], "proactive");
    assert!((now() - state["lastSwitchAt"].as_f64().unwrap()).abs() < 120.0);
}

#[test]
fn once_exit_code_no_action() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 50.0), (2, 10.0), (3, 10.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    let events = events(&out);
    assert_eq!(reasons(&events), ["below-threshold"]);
    assert_eq!(of_kind(&events, "no-switch")[0]["detail"], "50% < 90%");
    assert_eq!(live_token(&env), "at-1");
    assert!(!state_path(&env).exists());
}

#[test]
fn once_exit_code_blocked() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 95.0), (2, 86.0), (3, 88.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(3);
    assert_eq!(reasons(&events(&out)), ["no-qualifying-candidate"]);
    assert_eq!(live_token(&env), "at-1");
}

#[test]
fn once_reports_all_exhausted_with_the_earliest_reset() {
    let env = TestEnv::new();
    three(&env);
    let at = |pct: f64, reset: &str| {
        json!({"five_hour": {"utilization": pct, "resets_at": reset},
               "seven_day": {"utilization": 0.0}})
    };
    stub(
        &env,
        vec![
            (1, at(100.0, "2099-07-03T12:00:00+00:00")),
            (2, at(100.0, "2099-07-03T10:30:00+00:00")),
            (3, at(100.0, "2099-07-03T11:00:00+00:00")),
        ],
    );

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(3);
    let events = events(&out);
    let exhausted = of_kind(&events, "all-exhausted");
    assert_eq!(exhausted.len(), 1, "{}", out.stdout);
    assert_eq!(exhausted[0]["earliestResetAt"], "2099-07-03T10:30:00Z");
}

#[test]
fn json_stdout_is_pure_jsonl_with_python_separators() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 42.0), (2, 10.0), (3, 10.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    assert!(
        lines[0].starts_with("{\"schemaVersion\": 1, \"event\": \"poll\", \"ts\": \""),
        "{}",
        lines[0]
    );
    let poll: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(poll["active"], json!({"number": 1, "email": EMAILS[1]}));
    assert_eq!(
        poll["headroomPct"],
        json!({"1": 58.0, "2": 90.0, "3": null})
    );
    assert_eq!(poll.get("fetchErrors"), None);
    assert_eq!(poll["threshold"], 90.0);
    assert_eq!(poll["windowsPct"]["1"], json!({"5h": 42.0, "7d": 0.0}));
    assert!(lines[0].contains("\"headroomPct\": {\"1\": 58.0, \"2\": 90.0, \"3\": null}"));
    assert!(lines[0].contains("\"threshold\": 90.0"));
    assert!(lines[0].ends_with('}'));
    assert_eq!(
        lines[1]
            .split_once("\"ts\": ")
            .unwrap()
            .1
            .split_once(", ")
            .unwrap()
            .1,
        "\"reason\": \"below-threshold\", \"detail\": \"42% < 90%\"}"
    );
}

#[test]
fn unknown_usage_reports_fetch_errors() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 95.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(3);
    let events = events(&out);
    let poll = of_kind(&events, "poll")[0];
    assert_eq!(poll["headroomPct"], json!({"1": 5.0, "2": null, "3": null}));
    assert_eq!(
        poll["fetchErrors"],
        json!({"2": "http-500", "3": "http-500"})
    );
    assert_eq!(reasons(&events), ["no-comparison"]);
}

#[test]
fn human_output_is_timestamped_lines() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 95.0), (2, 40.0), (3, 20.0)]);

    let out = env.run(["auto", "--once"]);
    out.assert_code(0);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    for line in &lines {
        let (stamp, _) = line.split_once("  ").unwrap();
        assert_eq!(stamp.len(), 8, "{line}");
        assert_eq!(stamp.matches(':').count(), 2, "{line}");
    }
    assert!(
        lines[0].ends_with(
            "  Account-1 (a@example.com): 95% used (switch at 90%) | others: #2: 5h 40% · 7d 0%, #3: 5h 20% · 7d 0%"
        ),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with("  Switched Account-1 -> Account-3 (c@example.com) (proactive)"),
        "{}",
        lines[1]
    );
}

#[test]
fn flags_override_settings_json() {
    let env = TestEnv::new();
    three(&env);
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1,
                "autoswitch": {"threshold": 80.0, "cooldownSeconds": 42.0}}),
    );
    env.write_json(
        state_path(&env),
        &json!({"schemaVersion": 1, "lastSwitchAt": now() - 100.0}),
    );
    stub_five_hour(&env, &[(1, 65.0), (2, 10.0), (3, 30.0)]);

    let out = env.run(["auto", "--once", "--json", "--threshold", "60"]);
    out.assert_code(0);
    let events = events(&out);
    assert_eq!(of_kind(&events, "poll")[0]["threshold"], 60.0);
    assert_eq!(of_kind(&events, "switch")[0]["to"]["number"], 2);
}

#[test]
fn settings_cooldown_holds_a_proactive_switch() {
    let env = TestEnv::new();
    three(&env);
    let recent = now() - 10.0;
    env.write_json(
        state_path(&env),
        &json!({"schemaVersion": 1, "lastSwitchAt": recent}),
    );
    stub_five_hour(&env, &[(1, 95.0), (2, 10.0), (3, 10.0)]);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    assert_eq!(reasons(&events(&out)), ["cooldown"]);
    assert_eq!(state(&env)["lastSwitchAt"], recent);
    assert_eq!(live_token(&env), "at-1");

    let out = env.run(["auto", "--once", "--json", "--cooldown", "0"]);
    out.assert_code(0);
    assert_eq!(live_token(&env), "at-2");
}

#[test]
fn auto_picks_up_configured_threshold() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 50.0), (2, 10.0), (3, 10.0)]);
    env.run(["config", "set", "autoswitch.threshold", "77"])
        .assert_code(0);

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    let events = events(&out);
    assert_eq!(of_kind(&events, "poll")[0]["threshold"], 77.0);
    assert_eq!(of_kind(&events, "no-switch")[0]["detail"], "50% < 77%");
}

#[test]
fn threshold_flag_is_clamped_like_a_setting() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 40.0), (2, 10.0), (3, 10.0)]);

    let out = env.run(["auto", "--once", "--json", "--threshold", "20"]);
    out.assert_code(2);
    assert_eq!(of_kind(&events(&out), "poll")[0]["threshold"], 50.0);
}

#[test]
fn strategy_flag_selects_consume_first() {
    let env = TestEnv::new();
    three(&env);
    let weekly = |pct: f64, reset: &str| {
        json!({"five_hour": {"utilization": pct},
               "seven_day": {"utilization": pct, "resets_at": reset}})
    };
    stub(
        &env,
        vec![
            (1, weekly(20.0, "2099-01-08T00:00:00+00:00")),
            (2, weekly(10.0, "2099-01-05T00:00:00+00:00")),
            (3, weekly(10.0, "2099-01-10T00:00:00+00:00")),
        ],
    );

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    assert_eq!(reasons(&events(&out)), ["below-threshold"]);

    let out = env.run(["auto", "--once", "--json", "--strategy", "consume-first"]);
    out.assert_code(0);
    let events = events(&out);
    let switch = of_kind(&events, "switch");
    assert_eq!(switch[0]["trigger"], "consume-first", "{}", out.stdout);
    assert_eq!(switch[0]["to"]["number"], 2);
    assert_eq!(state(&env)["leftTrigger"], "consume-first");
}

#[test]
fn model_flag_binds_the_per_model_limit() {
    let env = TestEnv::new();
    three(&env);
    let fable = |pct: f64| {
        json!({"five_hour": {"utilization": 5.0}, "seven_day": {"utilization": 0.0},
               "limits": [{"scope": {"model": {"display_name": "Fable"}}, "percent": pct}]})
    };
    stub(
        &env,
        vec![(1, fable(100.0)), (2, fable(30.0)), (3, fable(60.0))],
    );

    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);

    let out = env.run(["auto", "--once", "--json", "--model", "Fable"]);
    out.assert_code(0);
    let events = events(&out);
    assert_eq!(
        of_kind(&events, "switch")[0]["to"]["number"],
        2,
        "{}",
        out.stdout
    );
    assert_eq!(
        of_kind(&events, "poll")[0]["windowsPct"]["2"],
        json!({"5h": 5.0, "7d": 0.0, "Fable": 30.0})
    );
}

#[test]
fn api_key_flag_pair_is_last_wins() {
    let env = TestEnv::new();
    three(&env);
    let mut data = env.sequence();
    data["accounts"]["2"]["kind"] = json!("api_key");
    env.write_json(env.store_dir().join("sequence.json"), &data);
    env.security().set_item(
        "claude-swap",
        &format!("account-2-{}", EMAILS[2]),
        "sk-ant-api03-key",
    );
    stub_five_hour(&env, &[(1, 100.0), (3, 100.0)]);

    let out = env.run([
        "auto",
        "--once",
        "--json",
        "--include-api-key-accounts",
        "--no-include-api-key-accounts",
    ]);
    out.assert_code(3);

    let out = env.run([
        "auto",
        "--once",
        "--json",
        "--no-include-api-key-accounts",
        "--include-api-key-accounts",
    ]);
    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "switch")[0]["to"]["number"], 2);
}

#[test]
fn dry_run_reports_and_mutates_nothing() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 95.0), (2, 40.0), (3, 20.0)]);
    let sequence_before = env.sequence();

    let out = env.run(["auto", "--once", "--json", "--dry-run"]);
    out.assert_code(0);
    let events = events(&out);
    let switch = of_kind(&events, "switch");
    assert_eq!(switch[0]["dryRun"], true);
    assert_eq!(switch[0]["to"], json!({"number": 3, "email": EMAILS[3]}));
    assert_eq!(live_token(&env), "at-1");
    assert!(!state_path(&env).exists());
    assert_eq!(env.sequence(), sequence_before);

    let human = env.run(["auto", "--once", "--dry-run"]);
    human.assert_code(0);
    assert!(
        human
            .stdout
            .contains("[dry-run] would switch Account-1 -> Account-3 (c@example.com) (proactive)"),
        "{}",
        human.stdout
    );
}

#[test]
fn no_active_account_is_no_action() {
    let env = TestEnv::new();
    let out = env.run(["auto", "--once", "--json"]);
    out.assert_code(2);
    let events = events(&out);
    assert_eq!(of_kind(&events, "poll")[0]["active"], Value::Null);
    assert_eq!(reasons(&events), ["no-active-account"]);
}

#[test]
fn unknown_flag_errors() {
    let env = TestEnv::new();
    let out = env.run(["auto", "--bogus"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains(&format!(
            "{} auto: error: unrecognized arguments: --bogus",
            PROG
        )),
        "{}",
        out.stderr
    );
}

#[test]
fn invalid_float_errors() {
    let env = TestEnv::new();
    let out = env.run(["auto", "--once", "--threshold", "abc"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .contains("error: argument --threshold: invalid float value: 'abc'"),
        "{}",
        out.stderr
    );
}

#[test]
fn invalid_strategy_errors() {
    let env = TestEnv::new();
    let out = env.run(["auto", "--once", "--strategy", "rotation"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains(
            "argument --strategy: invalid choice: 'rotation' (choose from 'best', 'consume-first')"
        ),
        "{}",
        out.stderr
    );
}

#[test]
fn auto_help() {
    let env = TestEnv::new();
    let out = env.run(["auto", "--help"]);
    out.assert_code(0);
    assert!(out.stdout.contains("--once"));
    assert!(out.stdout.contains("Exit codes"));
}

const LOOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn iso_in(seconds: f64) -> String {
    chrono::DateTime::from_timestamp((now() + seconds) as i64, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// Lets the loop reach its sleep first: upstream's SIGTERM handler deadlocks
/// when it lands while the main thread holds the wake event's lock.
fn signal_idle(run: &support::Running, sig: i32) {
    std::thread::sleep(std::time::Duration::from_millis(300));
    run.signal(sig);
}

fn epoch(iso: &str) -> f64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .unwrap()
        .timestamp() as f64
}

#[test]
fn loop_prints_its_banner_and_sigterm_exits_zero() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 10.0), (2, 10.0), (3, 10.0)]);

    let mut run = env.cmd(["auto"]).spawn();
    let banner = run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("Auto-switch running"));
    assert_eq!(
        banner.as_deref(),
        Some("Auto-switch running: threshold 90%, every 60s — Ctrl-C to stop")
    );
    assert!(
        run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("10% used"))
            .is_some()
    );
    signal_idle(&run, libc::SIGTERM);
    let out = run.finish(LOOP_TIMEOUT);
    out.assert_code(0);
    assert!(
        !out.stdout.contains("Auto-switch stopped"),
        "{}",
        out.stdout
    );
}

#[test]
fn loop_interval_flag_overrides_the_setting_and_is_clamped() {
    let env = TestEnv::new();
    three(&env);
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "autoswitch": {"intervalSeconds": 120.0}}),
    );
    stub_five_hour(&env, &[(1, 10.0), (2, 10.0), (3, 10.0)]);

    for (args, want) in [
        (vec!["auto", "--dry-run"], "every 120s (dry-run) —"),
        (vec!["auto", "--interval", "30"], "every 30s —"),
        (vec!["auto", "--interval", "5"], "every 15s —"),
    ] {
        let mut run = env.cmd(&args).spawn();
        let banner = run
            .wait_for_line(LOOP_TIMEOUT, |l| l.contains("Auto-switch running"))
            .unwrap_or_default();
        assert!(banner.contains(want), "{args:?}: {banner}");
        assert!(
            run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("10% used"))
                .is_some()
        );
        signal_idle(&run, libc::SIGTERM);
        run.finish(LOOP_TIMEOUT).assert_code(0);
    }
}

#[test]
fn loop_json_all_exhausted_sleeps_until_the_capped_recheck() {
    let env = TestEnv::new();
    three(&env);
    let at = |pct: f64| {
        json!({"five_hour": {"utilization": pct, "resets_at": iso_in(1800.0)},
               "seven_day": {"utilization": 0.0}})
    };
    stub(&env, vec![(1, at(100.0)), (2, at(100.0)), (3, at(100.0))]);

    let mut run = env.cmd(["auto", "--json", "--interval", "15"]).spawn();
    let sleep = run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("\"event\": \"sleep\""));
    let seen_at = now();
    signal_idle(&run, libc::SIGTERM);
    let out = run.finish(LOOP_TIMEOUT);
    out.assert_code(0);
    assert!(sleep.is_some(), "{}\n{}", out.stdout, out.stderr);
    let events = events(&out);
    let names: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["poll", "all-exhausted", "sleep"], "{}", out.stdout);
    let sleep = &events[2];
    assert_eq!(sleep["seconds"], json!(600.0));
    let until = sleep["until"].as_str().unwrap();
    assert!(until.ends_with('Z'), "{until}");
    assert!((epoch(until) - (seen_at + 600.0)).abs() < 15.0, "{sleep}");
    assert!(
        out.stdout
            .lines()
            .last()
            .unwrap()
            .starts_with("{\"schemaVersion\": 1, \"event\": \"sleep\", \"ts\": \"")
    );
}

#[test]
fn loop_normal_cadence_emits_no_sleep_event() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 10.0), (2, 10.0), (3, 10.0)]);

    let mut run = env.cmd(["auto", "--json"]).spawn();
    assert!(
        run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("below-threshold"))
            .is_some()
    );
    signal_idle(&run, libc::SIGTERM);
    let out = run.finish(LOOP_TIMEOUT);
    out.assert_code(0);
    assert_eq!(of_kind(&events(&out), "sleep").len(), 0, "{}", out.stdout);
}

#[test]
fn loop_ctrl_c_reports_stopped_and_exits_130() {
    let env = TestEnv::new();
    three(&env);
    stub_five_hour(&env, &[(1, 10.0), (2, 10.0), (3, 10.0)]);

    let mut run = env.cmd(["auto"]).spawn();
    assert!(
        run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("10% used"))
            .is_some()
    );
    signal_idle(&run, libc::SIGINT);
    let out = run.finish(LOOP_TIMEOUT);
    out.assert_code(130);
    assert!(
        out.stdout.ends_with("\nAuto-switch stopped\n"),
        "{:?}",
        out.stdout
    );

    let mut run = env.cmd(["auto", "--json"]).spawn();
    assert!(
        run.wait_for_line(LOOP_TIMEOUT, |l| l.contains("below-threshold"))
            .is_some()
    );
    signal_idle(&run, libc::SIGINT);
    let out = run.finish(LOOP_TIMEOUT);
    out.assert_code(130);
    assert_eq!(out.stderr, "\nAuto-switch stopped\n");
    events(&out);
}
