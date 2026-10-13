//! The passive update notice and `upgrade`.
//!
//! Ported from upstream tests/test_update_check.py::TestCheckForUpdate
//! (re-targeted from PyPI to this repository's GitHub Releases). The stub
//! answers in both shapes (`tag_name` for ccswap, `info.version` for
//! upstream) so the same cases run against both implementations.

mod support;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{StubResponse, TestEnv, is_upstream, prog};

const PATH: &str = "/update-check";

fn release(version: &str) -> StubResponse {
    StubResponse::json(
        200,
        &json!({"tag_name": format!("v{version}"), "info": {"version": version}}).to_string(),
    )
}

fn cache_path(env: &TestEnv) -> std::path::PathBuf {
    let name = if is_upstream() {
        "update_check.json"
    } else {
        "ccswap_update_check.json"
    };
    env.store_dir().join("cache").join(name)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn seed_cache(env: &TestEnv, age_secs: f64, data: Value) {
    env.write_json(
        cache_path(env),
        &json!({"timestamp": now() - age_secs, "data": data}),
    );
}

fn checks(env: &TestEnv) -> usize {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == PATH)
        .count()
}

fn current_version(env: &TestEnv) -> String {
    let out = env.run(["--version"]);
    out.stdout.trim().rsplit(' ').next().unwrap().to_string()
}

fn notice(env: &TestEnv, latest: &str) -> String {
    let (name, cli) = if is_upstream() {
        ("claude-swap", "cswap")
    } else {
        ("ccswap", "ccswap")
    };
    format!(
        "\nA newer version of {name} is available ({latest}). You are using {}. Run `{cli} upgrade` for upgrade instructions.\n",
        current_version(env)
    )
}

// test_newer_version_available
#[test]
fn a_newer_release_is_announced_on_stderr() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));

    let out = env.run(["status"]);

    out.assert_code(0);
    assert!(
        out.stderr.ends_with(&notice(&env, "99.0.0")),
        "{}",
        out.stderr
    );
    assert!(!out.stdout.contains("newer version"));
    let cache = env.read_json(cache_path(&env));
    assert_eq!(cache["data"], "99.0.0");
    assert!(cache["timestamp"].as_f64().unwrap() > now() - 60.0);
    let keys: Vec<&String> = cache.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["timestamp", "data"]);
}

#[test]
fn the_check_asks_github_for_the_latest_release() {
    if is_upstream() {
        return;
    }
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));

    env.run(["status"]).assert_code(0);

    let requests = env.http().requests();
    let request = requests.iter().find(|r| r.path() == PATH).unwrap();
    assert_eq!(request.method, "GET");
    assert!(
        request
            .header("User-Agent")
            .is_some_and(|ua| ua.starts_with("ccswap/")),
        "{:?}",
        request.headers
    );
    assert_eq!(
        request.header("Accept"),
        Some("application/vnd.github+json")
    );
}

#[test]
fn the_cache_file_does_not_collide_with_python_cswap() {
    if is_upstream() {
        return;
    }
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    let python_cache = env.store_dir().join("cache/update_check.json");
    env.write_json(
        &python_cache,
        &json!({"timestamp": now(), "data": "0.26.0"}),
    );

    env.run(["status"]).assert_code(0);

    assert_eq!(checks(&env), 1, "Python's cache is not ccswap's");
    assert_eq!(env.read_json(&python_cache)["data"], "0.26.0");
}

// test_already_on_latest
#[test]
fn an_older_release_is_not_announced() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("0.0.1"));

    let out = env.run(["status"]);

    out.assert_code(0);
    assert!(!out.stderr.contains("newer version"), "{}", out.stderr);
    assert_eq!(env.read_json(cache_path(&env))["data"], "0.0.1");
}

#[test]
fn a_tag_that_is_not_a_plain_version_is_ignored() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0rc1"));

    let out = env.run(["status"]);

    out.assert_code(0);
    assert!(!out.stderr.contains("newer version"), "{}", out.stderr);
}

// test_network_error_returns_none_and_caches
#[test]
fn a_failed_check_is_cached_as_null() {
    let env = TestEnv::new();

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(checks(&env), 1);
    assert!(!out.stderr.contains("newer version"));
    assert_eq!(env.read_json(cache_path(&env))["data"], Value::Null);
}

// test_fresh_error_cache_skips_network
#[test]
fn a_fresh_null_cache_skips_the_network() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    seed_cache(&env, 60.0, Value::Null);

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(checks(&env), 0);
    assert!(!out.stderr.contains("newer version"));
}

// test_fresh_cache_no_network
#[test]
fn a_fresh_cache_answers_without_the_network() {
    let env = TestEnv::new();
    seed_cache(&env, 60.0, json!("99.0.0"));

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(checks(&env), 0);
    assert!(
        out.stderr.ends_with(&notice(&env, "99.0.0")),
        "{}",
        out.stderr
    );
}

// test_stale_cache_fetches_from_pypi
#[test]
fn a_cache_older_than_a_day_is_refreshed() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    seed_cache(&env, 25.0 * 3600.0, json!("0.0.1"));

    let out = env.run(["status"]);

    out.assert_code(0);
    assert_eq!(checks(&env), 1);
    assert!(out.stderr.ends_with(&notice(&env, "99.0.0")));
    assert_eq!(env.read_json(cache_path(&env))["data"], "99.0.0");
}

#[test]
fn a_corrupt_cache_counts_as_missing() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    env.write_file(cache_path(&env), "{not json");

    env.run(["status"]).assert_code(0);

    assert_eq!(checks(&env), 1);
}

#[test]
fn a_slow_release_server_is_given_up_on_after_two_seconds() {
    let env = TestEnv::new();
    env.http().respond_with("GET", PATH, |_| {
        std::thread::sleep(Duration::from_secs(5));
        release("99.0.0")
    });

    let started = Instant::now();
    let out = env.run(["status"]);
    let elapsed = started.elapsed();

    out.assert_code(0);
    assert!(elapsed < Duration::from_millis(4500), "{elapsed:?}");
    assert!(!out.stderr.contains("newer version"));
    assert_eq!(env.read_json(cache_path(&env))["data"], Value::Null);
}

#[test]
fn json_output_never_checks() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));

    let out = env.run(["status", "--json"]);

    out.assert_code(0);
    assert_eq!(checks(&env), 0);
    assert!(!cache_path(&env).exists());
}

#[test]
fn a_failed_command_does_not_check() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));

    env.run(["switch", "1"]).assert_code(1);

    assert_eq!(checks(&env), 0);
}

#[test]
fn purge_does_not_recreate_the_store_for_the_cache() {
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    env.write_json(env.store_dir().join("sequence.json"), &json!({}));

    env.cmd(["purge"]).stdin("y\n").run().assert_code(0);

    assert_eq!(checks(&env), 0);
    assert!(!env.store_dir().exists());
}

#[test]
fn upgrade_prints_how_to_update_and_succeeds() {
    if is_upstream() {
        return;
    }
    let env = TestEnv::new();
    env.http().respond("GET", PATH, release("99.0.0"));
    env.write_file(env.store_dir().join("sequence.json"), "{torn");

    for verb in ["upgrade", "update"] {
        let out = env.run([verb]);
        out.assert_code(0);
        assert_eq!(
            out.stdout,
            "To upgrade ccswap, download the latest release from:\n  https://github.com/ashigirl96/ccswap/releases/latest\nor build it from source:\n  cargo install --git https://github.com/ashigirl96/ccswap ccswap\n"
        );
        assert_eq!(out.stderr, "");
    }
    assert_eq!(checks(&env), 0);
    assert!(!cache_path(&env).exists());
    assert_eq!(prog(), "ccswap");
}

#[test]
fn upgrade_works_as_root() {
    if is_upstream() || !support::sudo_available() {
        return;
    }
    let env = TestEnv::new();

    let out = env.cmd(["upgrade"]).sudo().run();

    out.assert_code(0);
    assert!(
        out.stdout.starts_with("To upgrade ccswap"),
        "{}",
        out.stdout
    );
}
