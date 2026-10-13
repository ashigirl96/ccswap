//! `switch --strategy best|next-available` and `--model`.
//!
//! Ported from upstream tests/test_switcher.py::TestUsageAwareSwitch,
//! test_best_strategy_ignores_disabled_candidate and
//! tests/test_cli.py (strategy/model flag validation, model fallback to
//! `autoswitch.model`, `models`/`modelSource` in `--json`). Usage is served
//! by the stub usage endpoint per bearer token instead of patching
//! `_usage_by_account`; a token without a stub answers 500 (usage unknown).

mod support;

use support::hint;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv, prog};

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

/// Slots `nums` with stored credentials and configs; `live` is the live
/// login while `activeAccountNumber` records `recorded`.
fn seed(env: &TestEnv, nums: &[u32], live: u32, recorded: u32) {
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
        "activeAccountNumber": recorded,
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

/// Slot, then its five-hour utilization and optional Fable limit.
type UsageRow = (u32, Option<(f64, Option<f64>)>);

/// Five-hour utilization per slot, plus an optional Fable weekly limit.
/// `None` leaves the slot's usage unknown.
fn stub_usage(env: &TestEnv, rows: &[UsageRow]) {
    let bodies: Vec<(String, String)> = rows
        .iter()
        .filter_map(|(n, u)| {
            let (five_hour, fable) = (*u)?;
            let mut body = json!({"five_hour": {"utilization": five_hour},
                                  "seven_day": {"utilization": 0.0}});
            if let Some(f) = fable {
                body["limits"] =
                    json!([{"scope": {"model": {"display_name": "Fable"}}, "percent": f}]);
            }
            Some((format!("Bearer at-{n}"), body.to_string()))
        })
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

fn five_hour(rows: &[(u32, f64)]) -> Vec<UsageRow> {
    rows.iter().map(|(n, p)| (*n, Some((*p, None)))).collect()
}

fn active(env: &TestEnv) -> Value {
    env.sequence()["activeAccountNumber"].clone()
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

#[test]
fn best_switches_to_more_headroom() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    stub_usage(&env, &five_hour(&[(1, 50.0), (2, 90.0), (3, 20.0)]));

    let out = env.run(["switch", "--strategy", "best", "--json"]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["switched"], true, "{payload}");
    assert_eq!(payload["strategy"], "best");
    assert_eq!(payload["reason"], "switched");
    assert_eq!(payload["to"], json!({"number": 3, "email": EMAILS[3]}));
    assert_eq!(payload.get("models"), None);
    assert_eq!(active(&env), 3);
    assert_eq!(live_token(&env), "at-3");
}

#[test]
fn best_stays_when_current_is_already_best() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    stub_usage(&env, &five_hour(&[(1, 89.0), (2, 100.0)]));

    let out = env.run(["switch", "--strategy", "best"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Already on the account with the most remaining quota (Account-1).\n"
    );
    assert_eq!(active(&env), 1);
    assert_eq!(live_token(&env), "at-1");
}

#[test]
fn best_noop_reasons_and_messages() {
    type Rows = Vec<UsageRow>;
    let cases: Vec<(&str, Rows, &str, &str, &str)> = vec![
        (
            "all exhausted",
            five_hour(&[(1, 100.0), (2, 100.0), (3, 100.0)]),
            "candidates-exhausted",
            "All accounts are at their 5h/7d limit — staying on Account-1.",
            "All accounts are at their 5h/7d limit — staying on Account-1.\n",
        ),
        (
            "current usage unavailable",
            vec![(1, None), (2, Some((10.0, None))), (3, Some((10.0, None)))],
            "usage-unavailable",
            "Current account usage is unavailable — staying on Account-1.",
            "Current account usage is unavailable — staying on Account-1. Run cswap --switch to rotate.\n",
        ),
        (
            "no candidate usage",
            vec![(1, Some((50.0, None))), (2, None), (3, None)],
            "usage-unavailable",
            "No other account has usage data to compare — staying on Account-1.",
            "No other account has usage data to compare — staying on Account-1. Run cswap --switch to rotate.\n",
        ),
        (
            "incomplete comparison",
            vec![(1, Some((50.0, None))), (2, Some((90.0, None))), (3, None)],
            "usage-unavailable",
            "No account with known usage has more remaining quota; some usage is unavailable — staying on Account-1.",
            "No account with known usage has more remaining quota; some usage is unavailable — staying on Account-1.\n",
        ),
        (
            "current exhausted with unknown candidate",
            vec![
                (1, Some((100.0, None))),
                (2, Some((100.0, None))),
                (3, None),
            ],
            "usage-unavailable",
            "No account with known usage has more remaining quota; some usage is unavailable — staying on Account-1.",
            "No account with known usage has more remaining quota; some usage is unavailable — staying on Account-1.\n",
        ),
    ];
    for (name, rows, reason, message, human) in cases {
        let env = TestEnv::new();
        seed(&env, &[1, 2, 3], 1, 1);
        stub_usage(&env, &rows);

        let out = env.run(["switch", "--strategy", "best", "--json"]);
        out.assert_code(0);
        let payload = out.json();
        assert_eq!(payload["switched"], false, "{name}: {payload}");
        assert_eq!(payload["reason"], reason, "{name}");
        assert_eq!(payload["message"], message, "{name}");
        assert_eq!(
            payload["from"],
            json!({"number": 1, "email": EMAILS[1]}),
            "{name}"
        );
        assert_eq!(payload["to"], payload["from"], "{name}");

        let out = env.run(["switch", "--strategy", "best"]);
        out.assert_code(0);
        assert_eq!(out.stdout, hint(human), "{name}");
        assert_eq!(active(&env), 1, "{name}");
        assert_eq!(live_token(&env), "at-1", "{name}");
    }
}

#[test]
fn best_strategy_ignores_disabled_candidate() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    let mut data = env.sequence();
    data["accounts"]["2"]["disabled"] = true.into();
    env.seed_sequence(data);
    stub_usage(&env, &five_hour(&[(1, 90.0), (2, 0.0)]));

    let out = env.run(["switch", "--strategy", "best", "--json"]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["reason"], "no-valid-target", "{payload}");
    assert_eq!(payload["strategy"], "best");
    assert_eq!(payload["warnings"], json!(["Skipped Account-2 (disabled)"]));
    assert_eq!(active(&env), 1);
}

#[test]
fn skip_exhausted_skips_limited_account() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    stub_usage(&env, &five_hour(&[(1, 0.0), (2, 100.0), (3, 20.0)]));

    let out = env.run(["switch", "--strategy", "next-available"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Skipping Account-2 (at 5h/7d limit)\n"),
        "{}",
        out.stdout
    );
    assert_eq!(active(&env), 3);
    assert_eq!(live_token(&env), "at-3");
}

#[test]
fn skip_exhausted_all_limited_stays_put() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    stub_usage(&env, &five_hour(&[(1, 0.0), (2, 100.0), (3, 100.0)]));

    let out = env.run(["switch", "--strategy", "next-available", "--json"]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["switched"], false, "{payload}");
    assert_eq!(payload["strategy"], "next-available");
    assert_eq!(payload["reason"], "candidates-exhausted");
    assert_eq!(
        payload["message"],
        "All other accounts are at their 5h/7d limit — staying on Account-1."
    );
    assert_eq!(
        payload["warnings"],
        json!([
            "Skipped Account-2 (at 5h/7d limit)",
            "Skipped Account-3 (at 5h/7d limit)"
        ])
    );

    let out = env.run(["switch", "--strategy", "next-available"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Skipping Account-2 (at 5h/7d limit)\nSkipping Account-3 (at 5h/7d limit)\n\
         All other accounts are at their 5h/7d limit — staying on Account-1.\n"
    );
    assert_eq!(active(&env), 1);
    assert_eq!(live_token(&env), "at-1");
}

#[test]
fn skip_exhausted_unknown_usage_is_not_skipped() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    stub_usage(&env, &[(1, None), (2, None)]);

    env.run(["switch", "--strategy", "next-available", "--json"])
        .assert_code(0);
    assert_eq!(active(&env), 2);
}

#[test]
fn next_available_anchors_on_live_account_under_drift() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 2, 1);
    stub_usage(&env, &five_hour(&[(1, 0.0), (2, 0.0), (3, 0.0)]));

    let out = env.run(["switch", "--strategy", "next-available", "--json"]);
    out.assert_code(0);
    assert_eq!(out.json()["to"]["number"], 3);
    assert_eq!(active(&env), 3);
}

#[test]
fn next_available_with_models_skips_and_names_the_window() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "autoswitch": {"model": "Fable"}}),
    );
    stub_usage(
        &env,
        &[
            (1, Some((0.0, Some(10.0)))),
            (2, Some((5.0, Some(100.0)))),
            (3, Some((20.0, Some(20.0)))),
        ],
    );

    let out = env.run(["switch", "--strategy", "next-available"]);
    out.assert_code(0);
    assert!(
        out.stdout.starts_with(
            "Using configured model limits: Fable (from autoswitch.model)\n\
             Skipping Account-2 (at Fable limit)\n"
        ),
        "{}",
        out.stdout
    );
    assert_eq!(active(&env), 3);
}

#[test]
fn next_available_without_models_ignores_scoped() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    stub_usage(
        &env,
        &[
            (1, Some((0.0, Some(10.0)))),
            (2, Some((5.0, Some(100.0)))),
            (3, Some((20.0, Some(20.0)))),
        ],
    );

    let out = env.run(["switch", "--strategy", "next-available"]);
    out.assert_code(0);
    assert!(
        !out.stdout.contains("Using configured model limits"),
        "{}",
        out.stdout
    );
    assert!(!out.stdout.contains("Skipping"), "{}", out.stdout);
    assert_eq!(active(&env), 2);
}

#[test]
fn best_with_models_folds_scoped_into_the_comparison() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 1, 1);
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "autoswitch": {"model": "Opus"}}),
    );
    stub_usage(
        &env,
        &[
            (1, Some((5.0, Some(90.0)))),
            (2, Some((5.0, Some(20.0)))),
            (3, Some((50.0, Some(80.0)))),
        ],
    );

    let out = env.run([
        "switch",
        "--strategy",
        "best",
        "--model",
        " Fable, fable",
        "--json",
    ]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["to"]["number"], 2, "{payload}");
    assert_eq!(payload["models"], json!(["Fable"]));
    assert_eq!(payload["modelSource"], "cli");
    let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "schemaVersion",
            "switched",
            "from",
            "to",
            "strategy",
            "reason",
            "message",
            "warnings",
            "models",
            "modelSource"
        ]
    );
    assert_eq!(active(&env), 2);
}

#[test]
fn best_human_announces_the_cli_model_source() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    stub_usage(
        &env,
        &[(1, Some((5.0, Some(90.0)))), (2, Some((5.0, Some(20.0))))],
    );

    let out = env.run(["switch", "--strategy", "best", "--model", "Fable"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Using configured model limits: Fable (from --model)\n"),
        "{}",
        out.stdout
    );
    assert_eq!(active(&env), 2);
}

#[test]
fn best_noop_json_keeps_inert_model_warning() {
    let typo = "model(s) Fabel match no account's usage windows (typo?)";
    for (rows, reason) in [
        ([(1, 0.0), (2, 50.0)], "already-best"),
        ([(1, 100.0), (2, 100.0)], "candidates-exhausted"),
    ] {
        let env = TestEnv::new();
        seed(&env, &[1, 2], 1, 1);
        let rows: Vec<_> = rows
            .iter()
            .map(|(n, p)| (*n, Some((*p, Some(10.0)))))
            .collect();
        stub_usage(&env, &rows);

        let out = env.run(["switch", "--strategy", "best", "--json", "--model", "Fabel"]);
        out.assert_code(0);
        let payload = out.json();
        assert_eq!(payload["reason"], reason, "{payload}");
        assert_eq!(payload["warnings"], json!([typo]));
        assert_eq!(payload["models"], json!(["Fabel"]));
        assert_eq!(payload["modelSource"], "cli");
        if reason == "candidates-exhausted" {
            assert_eq!(
                payload["message"],
                "All accounts are at their usage limits — staying on Account-1."
            );
        }
    }
}

#[test]
fn manual_strategies_warn_on_inert_model_name() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    stub_usage(
        &env,
        &[(1, Some((0.0, Some(10.0)))), (2, Some((5.0, Some(10.0))))],
    );

    let out = env.run(["switch", "--strategy", "next-available", "--model", "Fabel"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .contains("model(s) Fabel match no account's usage windows (typo?)\n"),
        "{}",
        out.stdout
    );

    let out = env.run(["switch", "--strategy", "best", "--model", "Fable"]);
    out.assert_code(0);
    assert!(!out.stdout.contains("typo"), "{}", out.stdout);
}

#[test]
fn plain_switch_ignores_the_configured_model() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1, 1);
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"schemaVersion": 1, "autoswitch": {"model": "Fable"}}),
    );

    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let payload = out.json();
    assert_eq!(payload["strategy"], "rotation");
    assert_eq!(payload.get("models"), None);
    assert!(env.http().requests().is_empty());
}

#[test]
fn strategy_and_model_flag_combinations_are_rejected() {
    let env = TestEnv::new();
    let p = prog();
    for (args, message) in [
        (
            vec!["--strategy", "best", "--list"],
            "--strategy can only be used with bare 'switch'",
        ),
        (
            vec!["list", "--strategy", "next-available"],
            "--strategy can only be used with bare 'switch'",
        ),
        (
            vec!["switch", "--model", "Fable"],
            "--model can only be used with 'switch --strategy best' or 'switch --strategy next-available'",
        ),
    ] {
        let out = env.run(&args);
        out.assert_code(2);
        assert_eq!(
            out.stderr,
            format!("usage: {p} <command> [args] [options]\n{p}: error: {message}\n"),
            "{args:?}"
        );
    }
}
