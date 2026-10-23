use super::*;
use serde_json::json;

const NOW: f64 = 1_800_000_000.0;
const WEEK: f64 = 7.0 * 86400.0;

fn iso(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .unwrap()
        .to_rfc3339()
}

fn account(number: &str, alias: &str, display: DisplayUsage) -> MenuAccount {
    MenuAccount {
        number: number.into(),
        email: "someone@example.com".into(),
        is_active: false,
        display,
        last_good: None,
        alias: alias.into(),
        disabled: false,
        fetched_at: None,
    }
}

fn snapshot(accounts: Vec<MenuAccount>) -> MenuSnapshot {
    MenuSnapshot {
        accounts,
        ..Default::default()
    }
}

fn feed_with_codex(accounts: Vec<MenuAccount>, codex: Vec<MenuAccount>) -> Value {
    let snapshot = MenuSnapshot {
        accounts,
        codex,
        ..Default::default()
    };
    feed(&snapshot, 90, 60, NOW)
}

fn feed_of(accounts: Vec<MenuAccount>) -> Value {
    feed(&snapshot(accounts), 90, 60, NOW)
}

#[test]
fn feed_lists_session_weekly_and_scoped_windows_in_order() {
    let usage = json!({
        "five_hour": {"pct": 72.0, "resets_at": iso(NOW + 3600.0)},
        "seven_day": {"pct": 41.0, "resets_at": iso(NOW + 2.0 * 86400.0)},
        "scoped": [{"name": "Fable", "pct": 63.0, "resets_at": iso(NOW + 2.0 * 86400.0)}],
    });
    let mut work = account("1", "work", DisplayUsage::Measured(usage));
    work.is_active = true;
    let out = feed_of(vec![work]);
    assert_eq!(
        out["accounts"][0],
        json!({
            "number": "1",
            "label": "work",
            "active": true,
            "disabled": false,
            "note": null,
            "windows": [
                {"name": "Current session", "pct": 72.0, "resets_at": NOW + 3600.0},
                {"name": "All models", "pct": 41.0, "resets_at": NOW + 2.0 * 86400.0},
                {"name": "Fable", "pct": 63.0, "resets_at": NOW + 2.0 * 86400.0},
            ],
        })
    );
}

#[test]
fn feed_carries_version_threshold_and_write_time() {
    let out = feed_of(Vec::new());
    assert_eq!(out["v"], 1);
    assert_eq!(out["written_at"], NOW);
    assert_eq!(out["threshold"], 90);
    assert_eq!(out["accounts"], json!([]));
}

#[test]
fn codex_accounts_are_listed_after_claude_in_the_same_shape() {
    let usage = json!({"five_hour": {"pct": 30.0, "resets_at": iso(NOW + 600.0)}});
    let mut plus = account("1", "", DisplayUsage::Measured(usage));
    plus.email = "me@openai.example".into();
    plus.is_active = true;
    let team = account("2", "team", DisplayUsage::Note("log in again".into()));
    let out = feed_with_codex(
        vec![account("1", "work", DisplayUsage::Unavailable)],
        vec![plus, team],
    );
    assert_eq!(out["accounts"][0]["label"], "work");
    assert_eq!(
        out["codex"],
        json!([
            {
                "number": "1",
                "label": "me",
                "alias": null,
                "email": "me@openai.example",
                "active": true,
                "disabled": false,
                "note": null,
                "windows": [
                    {"name": "Current session", "pct": 30.0, "resets_at": NOW + 600.0},
                ],
            },
            {
                "number": "2",
                "label": "team",
                "alias": "team",
                "email": "someone@example.com",
                "active": false,
                "disabled": false,
                "note": "log in again",
                "windows": [],
            },
        ])
    );
}

#[test]
fn without_codex_accounts_the_feed_has_no_codex_key() {
    let out = feed_with_codex(
        vec![account("1", "work", DisplayUsage::Unavailable)],
        Vec::new(),
    );
    let mut keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["accounts", "stale_after", "threshold", "v", "written_at"]
    );
}

#[test]
fn stale_after_is_three_refreshes_but_at_least_five_minutes() {
    assert_eq!(feed(&snapshot(Vec::new()), 90, 60, NOW)["stale_after"], 300);
    assert_eq!(
        feed(&snapshot(Vec::new()), 90, 300, NOW)["stale_after"],
        900
    );
}

#[test]
fn a_passed_weekly_reset_shows_as_reset_to_the_next_boundary() {
    let usage = json!({"seven_day": {"pct": 88.0, "resets_at": iso(NOW - 3600.0)}});
    let out = feed_of(vec![account("1", "", DisplayUsage::Measured(usage))]);
    assert_eq!(
        out["accounts"][0]["windows"],
        json!([{"name": "All models", "pct": 0.0, "resets_at": NOW - 3600.0 + WEEK}])
    );
}

#[test]
fn a_window_without_a_reset_time_has_null_resets_at() {
    let usage = json!({"five_hour": {"pct": 5.0}});
    let out = feed_of(vec![account("1", "", DisplayUsage::Measured(usage))]);
    assert_eq!(out["accounts"][0]["windows"][0]["resets_at"], Value::Null);
}

#[test]
fn label_falls_back_to_the_email_local_part() {
    let out = feed_of(vec![account("2", "", DisplayUsage::Unavailable)]);
    assert_eq!(out["accounts"][0]["label"], "someone");
}

#[test]
fn unmeasured_accounts_carry_a_note_and_no_windows() {
    let out = feed_of(vec![
        account("1", "a", DisplayUsage::Unavailable),
        account("2", "b", DisplayUsage::Note("token expired".into())),
    ]);
    assert_eq!(out["accounts"][0]["note"], "usage unavailable");
    assert_eq!(out["accounts"][1]["note"], "token expired");
    assert_eq!(out["accounts"][1]["windows"], json!([]));
}

#[test]
fn disabled_accounts_are_marked() {
    let mut lab = account("4", "lab", DisplayUsage::Unavailable);
    lab.disabled = true;
    assert_eq!(feed_of(vec![lab])["accounts"][0]["disabled"], true);
}

fn request(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(REQUEST_FILENAME);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn a_fresh_request_yields_its_account_and_is_consumed() {
    let dir = tempfile::tempdir().unwrap();
    let path = request(
        &dir,
        &format!(r#"{{"switch_to": "2", "at": {}}}"#, NOW - 5.0),
    );
    assert_eq!(
        take_request(&path, NOW),
        Some(Command::SwitchTo("2".into()))
    );
    assert!(!path.exists());
    assert_eq!(take_request(&path, NOW), None);
}

#[test]
fn a_codex_request_switches_the_codex_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = request(
        &dir,
        &format!(r#"{{"switch_to": "3", "provider": "codex", "at": {NOW}}}"#),
    );
    assert_eq!(
        take_request(&path, NOW),
        Some(Command::CodexSwitchTo("3".into()))
    );
}

#[test]
fn a_claude_provider_request_switches_the_claude_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = request(
        &dir,
        &format!(r#"{{"switch_to": "3", "provider": "claude", "at": {NOW}}}"#),
    );
    assert_eq!(
        take_request(&path, NOW),
        Some(Command::SwitchTo("3".into()))
    );
}

#[test]
fn stale_or_malformed_requests_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    for body in [
        format!(r#"{{"switch_to": "2", "at": {}}}"#, NOW - 120.0),
        format!(r#"{{"switch_to": "../2", "at": {NOW}}}"#),
        format!(r#"{{"switch_to": "", "at": {NOW}}}"#),
        r#"{"switch_to": "2"}"#.to_string(),
        format!(r#"{{"switch_to": "2", "provider": "gemini", "at": {NOW}}}"#),
        format!(r#"{{"switch_to": "2", "provider": 1, "at": {NOW}}}"#),
        format!(r#"{{"switch_to": "../2", "provider": "codex", "at": {NOW}}}"#),
        "not json".to_string(),
    ] {
        let path = request(&dir, &body);
        assert_eq!(take_request(&path, NOW), None, "{body}");
        assert!(!path.exists(), "{body}");
    }
}
