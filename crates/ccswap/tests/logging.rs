//! `claude-swap.log`: the switch record and `--debug`.
//!
//! Ported from upstream tests/test_logging_config.py::{test_setup_does_not_create_dir,
//! test_dir_is_created_on_first_log, test_debug_adds_console_handler}; the
//! per-command log lines come from upstream `switcher.py`'s `_logger` calls.

mod support;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn creds(tag: &str) -> String {
    json!({"claudeAiOauth": {
        "accessToken": format!("at-{tag}"),
        "refreshToken": format!("rt-{tag}"),
        "expiresAt": 9_999_999_999_000_i64,
    }})
    .to_string()
}

fn email(num: u32) -> String {
    ["", "a@example.com", "b@example.com"][num as usize].to_string()
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}")})
}

/// Slots 1 and 2, slot `active` live.
fn seed(env: &TestEnv, active: u32) {
    let record = |n: u32| {
        json!({"email": email(n), "uuid": format!("uuid-{n}"), "organizationUuid": "",
               "organizationName": "", "added": "2024-01-01T00:00:00Z"})
    };
    env.seed_sequence(json!({
        "activeAccountNumber": active,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {"1": record(1), "2": record(2)},
    }));
    for n in [1, 2] {
        env.security().set_item(
            BACKUP_SERVICE,
            &format!("account-{n}-{}", email(n)),
            &creds(&n.to_string()),
        );
        env.write_file(
            env.store_dir()
                .join(format!("configs/.claude-config-{n}-{}.json", email(n))),
            &json!({"oauthAccount": oauth_account(n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(active)}),
    );
    env.security().set_item(
        ACTIVE_OAUTH_SERVICE,
        KEYCHAIN_ACCOUNT,
        &creds(&active.to_string()),
    );
}

fn log_path(env: &TestEnv) -> std::path::PathBuf {
    env.store_dir().join("claude-swap.log")
}

fn log_text(env: &TestEnv) -> String {
    std::fs::read_to_string(log_path(env)).unwrap_or_default()
}

/// `(level, message)` of each record, after checking the Python
/// `%(asctime)s - %(levelname)s - %(message)s` shape.
fn records(env: &TestEnv) -> Vec<(String, String)> {
    log_text(env)
        .lines()
        .map(|line| {
            let (stamp, rest) = line.split_at(23);
            let shape: String = stamp
                .chars()
                .map(|c| if c.is_ascii_digit() { 'D' } else { c })
                .collect();
            assert_eq!(shape, "DDDD-DD-DD DD:DD:DD,DDD", "asctime in {line:?}");
            let rest = rest.strip_prefix(" - ").expect(line);
            let (level, message) = rest.split_once(" - ").expect(line);
            (level.to_string(), message.to_string())
        })
        .collect()
}

fn info(message: &str) -> (String, String) {
    ("INFO".into(), message.into())
}

#[test]
fn a_switch_is_recorded_step_by_step() {
    let env = TestEnv::new();
    seed(&env, 1);

    env.run(["switch", "2", "--json"]).assert_code(0);

    assert_eq!(
        records(&env),
        vec![
            info("Backed up account 1 (config only; credentials unchanged)"),
            info("Wrote target credentials"),
            info("Updated config file"),
            info("Switched from account 1 to 2"),
        ]
    );
}

#[test]
fn a_rolled_back_switch_records_the_failure_and_each_undone_step() {
    let env = TestEnv::new();
    seed(&env, 1);
    env.write_file(
        env.store_dir()
            .join("configs/.claude-config-2-b@example.com.json"),
        "not json",
    );

    env.run(["switch", "2", "--json"]).assert_code(1);

    let records = records(&env);
    assert_eq!(
        records[..2],
        [
            info("Backed up account 1 (config only; credentials unchanged)"),
            info("Wrote target credentials")
        ]
    );
    let (level, message) = &records[2];
    assert_eq!(level, "ERROR");
    assert!(
        message.starts_with("Switch failed: ") && message.ends_with(", attempting rollback"),
        "{message}"
    );
    assert_eq!(
        records[3..],
        [
            info("Rolled back step: credentials_written"),
            info("Rollback successful")
        ]
    );
}

// test_debug_adds_console_handler
#[test]
fn debug_echoes_records_to_stderr() {
    let env = TestEnv::new();
    seed(&env, 1);

    let out = env.run(["switch", "2", "--json", "--debug"]);
    out.assert_code(0);

    let echoed: Vec<&str> = out
        .stderr
        .lines()
        .filter(|l| l.starts_with("INFO: "))
        .collect();
    assert_eq!(
        echoed,
        [
            "INFO: Backed up account 1 (config only; credentials unchanged)",
            "INFO: Wrote target credentials",
            "INFO: Updated config file",
            "INFO: Switched from account 1 to 2",
        ]
    );
    assert_eq!(records(&env).len(), 4, "the file still gets every record");
}

#[test]
fn without_debug_nothing_is_echoed() {
    let env = TestEnv::new();
    seed(&env, 1);

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(0);
    assert_eq!(out.stderr, "");
}

// test_setup_does_not_create_dir
#[test]
fn a_command_that_logs_nothing_creates_no_store() {
    let env = TestEnv::new();

    env.run(["status", "--json"]).assert_code(0);

    assert!(!env.store_dir().exists());
}

// test_dir_is_created_on_first_log
#[test]
fn records_append_to_the_existing_log() {
    let env = TestEnv::new();
    seed(&env, 1);
    let earlier = "2024-01-01 00:00:00,000 - INFO - earlier\n";
    env.write_file(log_path(&env), earlier);

    env.run(["switch", "2", "--json"]).assert_code(0);

    let text = log_text(&env);
    assert!(text.starts_with(earlier), "{text}");
    assert_eq!(text.lines().count(), 5);
}

#[test]
fn a_full_log_rotates_into_three_generations() {
    let env = TestEnv::new();
    seed(&env, 1);
    let full = "x".repeat(1024 * 1024 - 10) + "\n";
    env.write_file(log_path(&env), &full);
    for (n, text) in [(1, "gen1\n"), (2, "gen2\n"), (3, "gen3\n")] {
        env.write_file(env.store_dir().join(format!("claude-swap.log.{n}")), text);
    }

    env.run(["switch", "2", "--json"]).assert_code(0);

    let read = |name: &str| std::fs::read_to_string(env.store_dir().join(name)).unwrap();
    assert_eq!(read("claude-swap.log.1"), full);
    assert_eq!(read("claude-swap.log.2"), "gen1\n");
    assert_eq!(read("claude-swap.log.3"), "gen2\n");
    assert!(!env.store_dir().join("claude-swap.log.4").exists());
    assert_eq!(records(&env).len(), 4);
}

#[test]
fn add_records_the_new_slot_and_its_refresh() {
    let env = TestEnv::new();
    env.seed_login("a@example.com", "org-1", "Acme");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds("A"));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &json!({"account": {"uuid": "uuid-a@example.com", "email": "a@example.com"},
                    "organization": {"uuid": "org-1"}})
            .to_string(),
        ),
    );

    env.run(["add"]).assert_code(0);
    env.run(["add"]).assert_code(0);

    assert_eq!(
        records(&env),
        vec![
            info("Added account 1: a@example.com (org: org-1)"),
            info("Updated credentials for account 1: a@example.com"),
        ]
    );
}

#[test]
fn add_token_records_the_new_slot_and_its_refresh() {
    let env = TestEnv::new();

    for token in ["sk-ant-oat01-abc", "sk-ant-oat01-def"] {
        env.run(["add-token", token, "--email", "user@example.com"])
            .assert_code(0);
    }

    assert_eq!(
        records(&env),
        vec![
            info("Added account 1 from token: user@example.com"),
            info("Updated token for account 1: user@example.com"),
        ]
    );
}

#[test]
fn disable_enable_and_remove_are_recorded() {
    let env = TestEnv::new();
    seed(&env, 1);

    env.run(["disable", "2"]).assert_code(0);
    env.run(["enable", "2"]).assert_code(0);
    env.cmd(["remove", "2"]).stdin("y\n").run().assert_code(0);

    let account_records: Vec<_> = records(&env)
        .into_iter()
        .filter(|(_, m)| !m.starts_with("Removed session profile"))
        .collect();
    assert_eq!(
        account_records,
        vec![
            info("Disabled account 2: b@example.com"),
            info("Enabled account 2: b@example.com"),
            info("Removed account 2: b@example.com"),
        ]
    );
}

#[test]
fn config_logs_settings_it_cannot_read() {
    let env = TestEnv::new();
    let path = env.write_file(env.store_dir().join("settings.json"), "[1]");
    let warning = format!("{} is not a JSON object; using defaults", path.display());

    let out = env.run(["config", "list"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        format!("{warning}\n"),
        "only the start-up read reaches stderr"
    );
    let expected = vec![("WARNING".to_string(), warning.clone()); 3];
    assert_eq!(records(&env), expected);

    let out = env.run(["config", "--debug", "get", "ui.theme"]);
    out.assert_code(0);
    assert_eq!(
        out.stderr,
        format!("{warning}\nWARNING: {warning}\nWARNING: {warning}\nWARNING: {warning}\n")
    );
}

#[test]
fn config_logs_an_unsupported_choice() {
    let env = TestEnv::new();
    env.write_json(
        env.store_dir().join("settings.json"),
        &json!({"autoswitch": {"strategy": "fastest"}, "ui": {"theme": 7}}),
    );

    env.run(["config", "list"]).assert_code(0);

    let warning = |m: &str| ("WARNING".to_string(), m.to_string());
    assert_eq!(
        records(&env),
        vec![
            warning("settings.json: unsupported autoswitch.strategy 'fastest'; using 'best'"),
            warning("settings.json: unsupported ui.theme 7; using 'auto'"),
        ]
    );
}

#[test]
fn swap_and_move_are_recorded() {
    let env = TestEnv::new();
    seed(&env, 1);

    env.run(["swap", "1", "2"]).assert_code(0);
    env.run(["move", "a@example.com", "5"]).assert_code(0);

    let renumbering: Vec<_> = records(&env)
        .into_iter()
        .filter(|(_, m)| m.starts_with("Swapped") || m.starts_with("Moved"))
        .collect();
    assert_eq!(
        renumbering,
        vec![
            info("Swapped slots: 1 (a@example.com) <-> 2 (b@example.com)"),
            info("Moved slot: 2 (a@example.com) -> 5"),
        ]
    );
}

#[test]
fn a_swap_that_fails_before_its_commit_logs_the_rollback() {
    let env = TestEnv::new();
    seed(&env, 1);
    let store = env.store_dir();
    env.write_file(store.join(".lock"), "");
    env.write_file(log_path(&env), "");
    std::fs::set_permissions(&store, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();

    let out = env.run(["swap", "1", "2"]);
    std::fs::set_permissions(&store, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();

    out.assert_code(1);
    let errors: Vec<_> = records(&env)
        .into_iter()
        .filter(|(level, _)| level == "ERROR")
        .collect();
    assert_eq!(
        errors,
        vec![(
            "ERROR".to_string(),
            "Swap 1 <-> 2 failed mid-write; restoring both slots".to_string()
        )]
    );
}

#[test]
fn a_forced_activation_logs_the_stash_and_the_activation() {
    let env = TestEnv::new();
    seed(&env, 1);

    env.run(["switch", "2", "--force", "--json"]).assert_code(0);

    let records = records(&env);
    let stash = records
        .iter()
        .find(|(_, m)| m.starts_with("Live credential does not belong"))
        .unwrap_or_else(|| panic!("{records:?}"));
    assert_eq!(stash.0, "WARNING");
    let (head, tail) = stash.1.split_once(": stashed as ").expect(&stash.1);
    assert_eq!(
        head,
        "Live credential does not belong to Account-1 (displaced-live-login)"
    );
    assert!(
        tail.ends_with(
            " (credentials mtime unknown). Something outside cswap rewrote the live login after the last switch."
        ),
        "{tail}"
    );
    assert!(
        records.contains(&info(
            "Activated account 2 (forced, backup of current login skipped)"
        )),
        "{records:?}"
    );
}

#[test]
fn debug_records_the_usage_response_and_unparseable_spend() {
    let env = TestEnv::new();
    seed(&env, 1);
    let body = r#"{"five_hour": {"utilization": 5}, "extra_usage": {"is_enabled": true, "used_credits": "x", "monthly_limit": 1, "utilization": 1}}"#;
    env.http()
        .respond("GET", "/api/oauth/usage", StubResponse::json(200, body));

    env.run(["list", "--debug"]).assert_code(0);

    let text = log_text(&env);
    assert!(
        text.contains(
            " - DEBUG - Usage API response: {\n  \"five_hour\": {\n    \"utilization\": 5\n  },"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            " - DEBUG - extra_usage parse failed: ValueError(\"could not convert string to float: 'x'\")\n"
        ),
        "{text}"
    );
}

#[test]
fn an_unparseable_roster_is_logged() {
    let env = TestEnv::new();
    let path = env.write_file(env.store_dir().join("sequence.json"), "{not json");

    env.run(["list", "--json"]).assert_code(1);

    let expected = (
        "WARNING".to_string(),
        format!("Invalid JSON in {}", path.display()),
    );
    assert!(records(&env).contains(&expected), "{:?}", records(&env));
}

#[test]
fn usage_fetch_failures_are_logged_without_the_email() {
    let env = TestEnv::new();
    seed(&env, 1);
    env.http().respond(
        "GET",
        "/api/oauth/usage",
        StubResponse::json(429, "{}").header("Retry-After", "120"),
    );

    env.run(["list"]).assert_code(0);

    let records = records(&env);
    for num in [1, 2] {
        let expected = (
            "WARNING".to_string(),
            format!(
                "Usage fetch failed for account {num}: http-429, retry-after 120s (usage-endpoint budget reached; backing off)"
            ),
        );
        assert!(records.contains(&expected), "{records:?}");
    }
}

#[test]
fn a_migration_that_cannot_finish_logs_why() {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [],
        "accounts": {"1": {"email": "a@example.com"}},
    }));
    env.security()
        .fail_command_for_service("find-generic-password", "claude-code", 51);

    env.run(["status", "--json"]).assert_code(0);

    let records = records(&env);
    assert!(
        records.iter().any(|(level, message)| level == "WARNING"
            && message.starts_with(
                "macos_keyring_to_security: read of account-1-a@example.com failed: "
            )),
        "{records:?}"
    );
}

#[test]
fn an_unreadable_backup_file_is_logged() {
    let env = TestEnv::new();
    seed(&env, 1);
    let enc = env.write_file(
        env.store_dir()
            .join("credentials/.creds-2-b@example.com.enc"),
        "c2VrcmV0",
    );
    std::fs::set_permissions(&enc, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();

    let out = env.run(["switch", "2", "--json"]);
    std::fs::set_permissions(&enc, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();

    out.assert_code(0);
    let records = records(&env);
    assert!(
        records.iter().any(|(level, message)| level == "WARNING"
            && message.starts_with("Failed to read credentials file: ")),
        "{records:?}"
    );
}
