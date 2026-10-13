//! `swap` / `move`, ported from upstream tests/test_swap_accounts.py and
//! tests/test_move_accounts.py (test names keep upstream's `test_<name>`).
//! Upstream's monkeypatched faults are reproduced from outside: a read-only
//! store dir fails the sequence.json commit, a failing
//! `delete-generic-password` fails a required clear, and chmod 000 makes a
//! backup unreadable.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestEnv, prog};

const BACKUP_SERVICE: &str = "claude-swap";
const SAME_EMAIL: &str = "user@example.com";

fn sample_sequence_data() -> Value {
    json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": "account1@example.com", "uuid": "uuid-1", "added": "2024-01-01T00:00:00Z"},
            "2": {"email": "account2@example.com", "uuid": "uuid-2", "added": "2024-01-02T00:00:00Z"},
        },
    })
}

/// The sample with org fields already present, so no migration write runs.
fn sample_sequence_data_migrated() -> Value {
    let mut data = sample_sequence_data();
    for num in ["1", "2"] {
        let record = &mut data["accounts"][num];
        record["organizationUuid"] = "".into();
        record["organizationName"] = "".into();
    }
    data
}

fn sample_sequence_data_with_org() -> Value {
    json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {
                "email": SAME_EMAIL, "uuid": "user-uuid",
                "organizationUuid": "org-uuid-5678", "organizationName": "Acme Corp",
                "added": "2024-01-01T00:00:00Z",
            },
            "2": {
                "email": SAME_EMAIL, "uuid": "user-uuid",
                "organizationUuid": "", "organizationName": "",
                "added": "2024-01-02T00:00:00Z",
            },
        },
    })
}

fn seeded(data: Value) -> TestEnv {
    let env = TestEnv::new();
    env.seed_sequence(data);
    env
}

fn sequence(env: &TestEnv) -> Value {
    env.read_json(env.store_dir().join("sequence.json"))
}

fn enc_path(env: &TestEnv, num: &str, email: &str) -> PathBuf {
    env.store_dir()
        .join("credentials")
        .join(format!(".creds-{num}-{email}.enc"))
}

fn config_path(env: &TestEnv, num: &str, email: &str) -> PathBuf {
    env.store_dir()
        .join("configs")
        .join(format!(".claude-config-{num}-{email}.json"))
}

fn write_creds(env: &TestEnv, num: &str, email: &str, creds: &str) {
    env.security()
        .set_item(BACKUP_SERVICE, &format!("account-{num}-{email}"), creds);
}

fn write_enc(env: &TestEnv, num: &str, email: &str, creds: &str) -> PathBuf {
    env.write_file(enc_path(env, num, email), &base64(creds.as_bytes()))
}

/// The backup as the store reads it: a non-empty `.enc` wins, else the Keychain.
fn read_creds(env: &TestEnv, num: &str, email: &str) -> String {
    if let Ok(text) = std::fs::read_to_string(enc_path(env, num, email)) {
        let decoded = unbase64(text.trim());
        if !decoded.is_empty() {
            return decoded;
        }
    }
    env.security()
        .item(BACKUP_SERVICE, &format!("account-{num}-{email}"))
        .unwrap_or_default()
}

fn write_config(env: &TestEnv, num: &str, email: &str, config: &str) {
    env.write_file(config_path(env, num, email), config);
}

fn read_config(env: &TestEnv, num: &str, email: &str) -> String {
    std::fs::read_to_string(config_path(env, num, email)).unwrap_or_default()
}

fn session_dir(env: &TestEnv, num: &str, slug: &str) -> PathBuf {
    env.store_dir()
        .join("sessions")
        .join(format!("{num}-{slug}"))
}

fn staging_files(env: &TestEnv) -> Vec<String> {
    std::fs::read_dir(env.store_dir().join("credentials"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".swap-staging-"))
        .collect()
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Makes the sequence.json commit fail while the lock file stays usable.
fn make_store_root_read_only(env: &TestEnv) {
    for name in [".lock", "claude-swap.log"] {
        env.write_file(env.store_dir().join(name), "");
    }
    chmod(&env.store_dir(), 0o500);
}

fn accounts_email(data: &Value, num: &str) -> Value {
    data["accounts"][num]["email"].clone()
}

// -- swap ------------------------------------------------------------------

#[test]
fn test_swap_by_number() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["swap", "1", "2"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Swapped Account 1 and Account 2:\n  1: account2@example.com\n  2: account1@example.com\n"
    );
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "1"), "account2@example.com");
    assert_eq!(accounts_email(&data, "2"), "account1@example.com");
}

#[test]
fn test_swap_moves_active_number_with_account() {
    let env = seeded(sample_sequence_data());
    env.run(["swap", "1", "2"]).assert_code(0);
    let data = sequence(&env);
    assert_eq!(data["activeAccountNumber"], 2);
    assert_eq!(accounts_email(&data, "2"), "account1@example.com");
}

#[test]
fn test_swap_keeps_sequence_sorted() {
    let env = seeded(sample_sequence_data());
    env.run(["swap", "1", "2"]).assert_code(0);
    assert_eq!(sequence(&env)["sequence"], json!([1, 2]));
}

#[test]
fn test_swap_by_email_and_alias() {
    let mut data = sample_sequence_data();
    data["accounts"]["2"]["alias"] = "dev".into();
    let env = seeded(data);
    let out = env.run(["swap", "account1@example.com", "dev"]);
    out.assert_code(0);
    assert!(out.stdout.starts_with("Swapped Account 1 and Account 2:"));
    let data = sequence(&env);
    assert_eq!(data["accounts"]["1"]["alias"], "dev");
    assert!(data["accounts"]["2"].get("alias").is_none());
}

#[test]
fn test_swap_moves_credential_and_config_backups() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "1", "account1@example.com", "creds-one");
    write_config(&env, "1", "account1@example.com", "config-one");
    write_creds(&env, "2", "account2@example.com", "creds-two");
    write_config(&env, "2", "account2@example.com", "config-two");

    env.run(["swap", "1", "2"]).assert_code(0);

    assert_eq!(read_creds(&env, "2", "account1@example.com"), "creds-one");
    assert_eq!(read_config(&env, "2", "account1@example.com"), "config-one");
    assert_eq!(read_creds(&env, "1", "account2@example.com"), "creds-two");
    assert_eq!(read_config(&env, "1", "account2@example.com"), "config-two");
    assert_eq!(read_creds(&env, "1", "account1@example.com"), "");
    assert_eq!(read_creds(&env, "2", "account2@example.com"), "");
    assert_eq!(read_config(&env, "1", "account1@example.com"), "");
}

#[test]
fn test_swap_with_one_slot_missing_backups() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "1", "account1@example.com", "creds-one");
    env.run(["swap", "1", "2"]).assert_code(0);
    assert_eq!(read_creds(&env, "2", "account1@example.com"), "creds-one");
    assert_eq!(read_creds(&env, "1", "account2@example.com"), "");
}

#[test]
fn test_swap_same_account_rejected() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["swap", "1", "1"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Cannot swap an account with itself\n");
}

#[test]
fn test_swap_unknown_identifier_rejected() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["swap", "1", "nosuch@example.com"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: No account found with identifier: nosuch@example.com\n"
    );
    let out = env.run(["swap", "1", "7"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Account-7 does not exist\n");
}

#[test]
fn test_swap_same_email_accounts() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    write_creds(&env, "2", SAME_EMAIL, "creds-personal");

    env.run(["swap", "1", "2"]).assert_code(0);

    assert_eq!(read_creds(&env, "1", SAME_EMAIL), "creds-personal");
    assert_eq!(read_creds(&env, "2", SAME_EMAIL), "creds-org");
    assert_eq!(
        sequence(&env)["accounts"]["2"]["organizationUuid"],
        "org-uuid-5678"
    );
    assert!(staging_files(&env).is_empty());
}

#[test]
fn test_swap_same_email_rollback_restores_empty_slot() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    make_store_root_read_only(&env);

    let out = env.run(["swap", "1", "2"]);
    chmod(&env.store_dir(), 0o700);

    assert_eq!(out.code, 1, "stderr:\n{}", out.stderr);
    assert_eq!(read_creds(&env, "1", SAME_EMAIL), "creds-org");
    assert_eq!(read_creds(&env, "2", SAME_EMAIL), "");
    let data = sequence(&env);
    assert_eq!(data["accounts"]["1"]["organizationUuid"], "org-uuid-5678");
    assert_eq!(data["activeAccountNumber"], 1);
    assert!(staging_files(&env).is_empty());
}

#[test]
fn test_swap_same_email_one_sided_clears_destination() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");

    env.run(["swap", "1", "2"]).assert_code(0);

    assert_eq!(read_creds(&env, "2", SAME_EMAIL), "creds-org");
    assert_eq!(read_creds(&env, "1", SAME_EMAIL), "");
    assert_eq!(
        sequence(&env)["accounts"]["2"]["organizationUuid"],
        "org-uuid-5678"
    );
}

#[test]
fn test_swap_clears_stale_destination_key() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "2", "account1@example.com", "stale-foreign");
    env.run(["swap", "1", "2"]).assert_code(0);
    assert_eq!(read_creds(&env, "2", "account1@example.com"), "");
}

#[test]
fn test_swap_refuses_leftover_staging() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    write_creds(&env, "2", SAME_EMAIL, "creds-personal");
    let leftover = env.write_file(
        env.store_dir()
            .join("credentials/.swap-staging-creds-1.json"),
        "only-surviving-copy",
    );

    let out = env.run(["swap", "1", "2"]);
    out.assert_code(1);
    assert!(out.stderr.contains("interrupted swap"), "{}", out.stderr);
    assert_eq!(
        std::fs::read_to_string(&leftover).unwrap(),
        "only-surviving-copy"
    );
    assert_eq!(read_creds(&env, "1", SAME_EMAIL), "creds-org");
    assert_eq!(read_creds(&env, "2", SAME_EMAIL), "creds-personal");
    assert_eq!(
        sequence(&env)["accounts"]["1"]["organizationUuid"],
        "org-uuid-5678"
    );
    assert_eq!(staging_files(&env), vec![".swap-staging-creds-1.json"]);
}

/// Also covers test_swap_same_email_persistent_failure_keeps_staged_copy:
/// the Keychain delete keeps failing during the rollback, so the staged
/// pre-swap copy is kept (0600) and reported.
#[test]
fn test_swap_failed_required_clear_aborts_commit() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    env.security().fail_command("delete-generic-password", 51);

    let out = env.run(["swap", "1", "2"]);

    out.assert_code(1);
    assert!(
        out.stderr.contains("aborting before commit"),
        "{}",
        out.stderr
    );
    assert!(
        out.stdout
            .contains("Swap rollback was incomplete; your pre-swap credentials are preserved in:"),
        "{}",
        out.stdout
    );
    let data = sequence(&env);
    assert_eq!(data["accounts"]["1"]["organizationUuid"], "org-uuid-5678");
    assert_eq!(data["activeAccountNumber"], 1);
    assert_eq!(read_creds(&env, "1", SAME_EMAIL), "creds-org");
    let staged = env
        .store_dir()
        .join("credentials/.swap-staging-creds-1.json");
    assert_eq!(std::fs::read_to_string(&staged).unwrap(), "creds-org");
    assert_eq!(
        std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn test_swap_same_email_clears_prev_generations() {
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    write_creds(&env, "2", SAME_EMAIL, "creds-personal");

    env.run(["swap", "1", "2"]).assert_code(0);

    let prev_files: Vec<_> = std::fs::read_dir(env.store_dir().join("credentials"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".enc.prev"))
        .collect();
    assert!(prev_files.is_empty());
    let prev_items: Vec<_> = env
        .security()
        .items()
        .into_iter()
        .filter(|(_, account, _)| account.ends_with(".prev"))
        .collect();
    assert!(prev_items.is_empty(), "{prev_items:?}");
}

#[test]
fn test_swap_moves_session_profiles() {
    let env = seeded(sample_sequence_data());
    let session_a = session_dir(&env, "1", "account1_example.com");
    env.write_file(session_a.join("marker.txt"), "history-of-account-one");

    env.run(["swap", "1", "2"]).assert_code(0);

    let moved = session_dir(&env, "2", "account1_example.com");
    assert_eq!(
        std::fs::read_to_string(moved.join("marker.txt")).unwrap(),
        "history-of-account-one"
    );
    assert!(!session_a.exists());
}

#[test]
fn swap_moves_both_same_email_session_profiles() {
    let env = seeded(sample_sequence_data_with_org());
    env.write_file(session_dir(&env, "1", "user_example.com").join("m"), "org");
    env.write_file(
        session_dir(&env, "2", "user_example.com").join("m"),
        "personal",
    );

    env.run(["swap", "1", "2"]).assert_code(0);

    let read = |num| {
        std::fs::read_to_string(session_dir(&env, num, "user_example.com").join("m")).unwrap()
    };
    assert_eq!(read("1"), "personal");
    assert_eq!(read("2"), "org");
}

#[test]
fn test_unreadable_enc_aborts_the_swap_before_anything_changes() {
    let env = seeded(sample_sequence_data());
    write_enc(&env, "1", "account1@example.com", "rt-1");
    let enc = write_enc(&env, "2", "account2@example.com", "rt-2");
    chmod(&enc, 0o000);

    let out = env.run(["swap", "1", "2"]);
    chmod(&enc, 0o600);

    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: Account-2's stored credential could not be read (keychain unavailable?); nothing was changed. Retry once it is readable again.\n"
    );
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "1"), "account1@example.com");
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert_eq!(read_creds(&env, "1", "account1@example.com"), "rt-1");
    assert_eq!(read_creds(&env, "2", "account2@example.com"), "rt-2");
}

#[test]
fn test_absent_source_still_swaps() {
    let env = seeded(sample_sequence_data());
    env.run(["swap", "1", "2"]).assert_code(0);
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "1"), "account2@example.com");
    assert_eq!(accounts_email(&data, "2"), "account1@example.com");
}

#[test]
fn swap_without_a_store_is_an_error() {
    let env = TestEnv::new();
    let out = env.run(["swap", "1", "2"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: No accounts are managed yet\n");
}

#[test]
fn swap_and_move_refuse_inside_a_session_profile() {
    let env = seeded(sample_sequence_data());
    let profile = session_dir(&env, "1", "account1_example.com");
    std::fs::create_dir_all(&profile).unwrap();
    for args in [["swap", "1", "2"], ["move", "1", "5"]] {
        let out = env
            .cmd(args)
            .env("CLAUDE_CONFIG_DIR", profile.to_str().unwrap())
            .run();
        out.assert_code(1);
        assert!(
            out.stderr
                .starts_with("Error: This shell is inside a cswap run session profile"),
            "{}",
            out.stderr
        );
    }
    assert_eq!(accounts_email(&sequence(&env), "1"), "account1@example.com");
}

#[test]
fn swap_argument_errors_match_argparse() {
    let env = TestEnv::new();
    let p = prog();
    let usage = format!("usage: {p} swap [-h] [--debug] NUM|EMAIL|ALIAS NUM|EMAIL|ALIAS\n");
    let out = env.run(["swap"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!(
            "{usage}{p} swap: error: the following arguments are required: NUM|EMAIL|ALIAS, NUM|EMAIL|ALIAS\n"
        )
    );
    let out = env.run(["swap", "1", "2", "3"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!("{usage}{p} swap: error: unrecognized arguments: 3\n")
    );
    let out = env.run(["swap", "--debug=1", "1", "2"]);
    assert_eq!(
        out.stderr,
        format!("{usage}{p} swap: error: argument --debug: ignored explicit argument '1'\n")
    );
    let out = env.run(["swap", "--he"]);
    out.assert_code(0);
    assert!(out.stdout.starts_with(&usage), "{}", out.stdout);
    assert!(
        out.stdout
            .contains(&format!("  {p} swap dev user@example.com\n"))
    );
}

// -- move ------------------------------------------------------------------

#[test]
fn test_move_to_empty_slot_relocates() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "2", "5"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Moved account2@example.com to slot 5\n");
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "5"), "account2@example.com");
    assert!(data["accounts"].get("2").is_none());
    assert_eq!(accounts_email(&data, "1"), "account1@example.com");
}

#[test]
fn test_move_to_empty_slot_updates_rotation_sequence() {
    let env = seeded(sample_sequence_data());
    env.run(["move", "2", "5"]).assert_code(0);
    assert_eq!(sequence(&env)["sequence"], json!([1, 5]));
}

#[test]
fn test_move_keeps_sequence_sorted() {
    let env = seeded(sample_sequence_data());
    env.run(["move", "1", "5"]).assert_code(0);
    assert_eq!(sequence(&env)["sequence"], json!([2, 5]));
}

#[test]
fn test_move_unbacked_account_clears_stale_target_key() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "5", "account2@example.com", "stale-foreign");
    env.run(["move", "2", "5"]).assert_code(0);
    assert_eq!(read_creds(&env, "5", "account2@example.com"), "");
    assert_eq!(accounts_email(&sequence(&env), "5"), "account2@example.com");
}

#[test]
fn test_move_failed_required_clear_aborts_commit() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "5", "account2@example.com", "stale-foreign");
    env.security().fail_command("delete-generic-password", 51);

    let out = env.run(["move", "2", "5"]);

    out.assert_code(1);
    assert!(
        out.stderr.contains("aborting before commit"),
        "{}",
        out.stderr
    );
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert!(data["accounts"].get("5").is_none());
}

#[test]
fn test_move_strict_clear_fails_closed_on_unreadable_dir() {
    let env = seeded(sample_sequence_data_migrated());
    write_enc(&env, "5", "account2@example.com", "stale-foreign");
    let credentials = env.store_dir().join("credentials");
    chmod(&credentials, 0o000);

    let out = env.run(["move", "2", "5"]);
    chmod(&credentials, 0o700);

    out.assert_code(1);
    assert!(out.stderr.contains("could not be read"), "{}", out.stderr);
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert!(data["accounts"].get("5").is_none());
    assert_eq!(
        read_creds(&env, "5", "account2@example.com"),
        "stale-foreign"
    );
}

#[test]
fn test_move_strict_clear_fails_closed_on_locked_keychain() {
    let env = seeded(sample_sequence_data_migrated());
    write_creds(&env, "5", "account2@example.com", "stale-keychain");
    env.security().fail_with(51);

    let out = env.run(["move", "2", "5"]);

    out.assert_code(1);
    assert!(out.stderr.contains("could not be read"), "{}", out.stderr);
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert!(data["accounts"].get("5").is_none());
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-5-account2@example.com")
            .as_deref(),
        Some("stale-keychain")
    );
}

#[test]
fn test_move_metadata_failure_leaves_account_intact() {
    let env = seeded(sample_sequence_data_migrated());
    write_creds(&env, "2", "account2@example.com", "creds-two");
    make_store_root_read_only(&env);

    let out = env.run(["move", "2", "5"]);
    chmod(&env.store_dir(), 0o700);

    assert_eq!(out.code, 1, "stderr:\n{}", out.stderr);
    assert_eq!(read_creds(&env, "2", "account2@example.com"), "creds-two");
    assert_eq!(read_creds(&env, "5", "account2@example.com"), "");
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert!(data["accounts"].get("5").is_none());
}

#[test]
fn test_move_active_account_to_empty_slot_follows_active() {
    let env = seeded(sample_sequence_data());
    env.run(["move", "1", "9"]).assert_code(0);
    let data = sequence(&env);
    assert_eq!(data["activeAccountNumber"], 9);
    assert_eq!(accounts_email(&data, "9"), "account1@example.com");
}

#[test]
fn test_move_by_email_and_alias() {
    let mut data = sample_sequence_data();
    data["accounts"]["2"]["alias"] = "dev".into();
    let env = seeded(data);
    env.run(["move", "dev", "7"]).assert_code(0);
    let data = sequence(&env);
    assert_eq!(data["accounts"]["7"]["alias"], "dev");
    assert!(data["accounts"].get("2").is_none());
}

#[test]
fn test_move_relocates_credential_and_config_backups() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "2", "account2@example.com", "creds-two");
    write_config(&env, "2", "account2@example.com", "config-two");

    env.run(["move", "2", "5"]).assert_code(0);

    assert_eq!(read_creds(&env, "5", "account2@example.com"), "creds-two");
    assert_eq!(read_config(&env, "5", "account2@example.com"), "config-two");
    assert_eq!(read_creds(&env, "2", "account2@example.com"), "");
    assert_eq!(read_config(&env, "2", "account2@example.com"), "");
}

#[test]
fn test_move_relocates_session_profile() {
    let env = seeded(sample_sequence_data());
    let session = session_dir(&env, "2", "account2_example.com");
    env.write_file(session.join("marker.txt"), "history-of-account-two");

    env.run(["move", "2", "5"]).assert_code(0);

    let moved = session_dir(&env, "5", "account2_example.com");
    assert_eq!(
        std::fs::read_to_string(moved.join("marker.txt")).unwrap(),
        "history-of-account-two"
    );
    assert!(!session.exists());
}

#[test]
fn move_drops_the_relocated_profiles_stale_credentials() {
    let env = seeded(sample_sequence_data());
    write_creds(&env, "2", "account2@example.com", "creds-two");
    let session = session_dir(&env, "2", "account2_example.com");
    env.write_file(session.join(".credentials.json"), "seeded-from-old-slot");

    env.run(["move", "2", "5"]).assert_code(0);

    let moved = session_dir(&env, "5", "account2_example.com");
    assert!(moved.is_dir());
    assert!(!moved.join(".credentials.json").exists());
}

#[test]
fn test_move_to_empty_slot_with_missing_backups() {
    let env = seeded(sample_sequence_data());
    env.run(["move", "2", "5"]).assert_code(0);
    assert_eq!(accounts_email(&sequence(&env), "5"), "account2@example.com");
    assert_eq!(read_creds(&env, "5", "account2@example.com"), "");
}

#[test]
fn test_move_to_occupied_slot_swaps() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "1", "2"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "Swapped Account 1 and Account 2:\n  1: account2@example.com\n  2: account1@example.com\n"
    );
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account1@example.com");
    assert_eq!(accounts_email(&data, "1"), "account2@example.com");
}

#[test]
fn test_move_is_general_form_of_swap() {
    let moved = seeded(sample_sequence_data());
    moved.run(["move", "1", "2"]).assert_code(0);
    let swapped = seeded(sample_sequence_data());
    swapped.run(["swap", "1", "2"]).assert_code(0);
    for num in ["1", "2"] {
        assert_eq!(
            accounts_email(&sequence(&moved), num),
            accounts_email(&sequence(&swapped), num)
        );
    }
}

#[test]
fn test_move_to_same_slot_is_noop() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "1", "1"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Already in slot 1: account1@example.com\n");
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "1"), "account1@example.com");
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
}

#[test]
fn test_move_normalizes_padded_target() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "2", "05"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "Moved account2@example.com to slot 5\n");
    assert_eq!(accounts_email(&sequence(&env), "5"), "account2@example.com");
}

#[test]
fn test_move_invalid_target_rejected() {
    let env = seeded(sample_sequence_data());
    for bad in ["abc", "0", "-1", "1.5", ""] {
        let out = env.run(["move", "1", bad]);
        out.assert_code(1);
        assert_eq!(
            out.stderr,
            format!(
                "Error: Target slot must be a positive slot number, got: '{bad}' (use `swap` to trade two accounts by identifier)\n"
            )
        );
    }
}

#[test]
fn test_move_unknown_account_rejected() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "nosuch@example.com", "5"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: No account found with identifier: nosuch@example.com\n"
    );
}

#[test]
fn test_move_target_above_cap_rejected() {
    let env = seeded(sample_sequence_data());
    let out = env.run(["move", "1", "100"]);
    out.assert_code(1);
    assert_eq!(
        out.stderr,
        "Error: Target slot 100 is out of range (1-99): new accounts are numbered from the highest slot, so a large target would inflate future account numbers\n"
    );
}

#[test]
fn test_move_cap_stretches_to_existing_max_slot() {
    let mut data = sample_sequence_data();
    data["accounts"]["150"] = json!({
        "email": "account150@example.com", "uuid": "uuid-150", "added": "2024-01-03T00:00:00Z",
    });
    data["sequence"] = json!([1, 2, 150]);
    let env = seeded(data);

    env.run(["move", "1", "120"]).assert_code(0);
    assert_eq!(
        accounts_email(&sequence(&env), "120"),
        "account1@example.com"
    );

    let out = env.run(["move", "2", "151"]);
    out.assert_code(1);
    assert!(
        out.stderr.contains("out of range (1-150)"),
        "{}",
        out.stderr
    );
}

#[test]
fn test_unreadable_enc_aborts_the_move_before_anything_changes() {
    let env = seeded(sample_sequence_data());
    let enc = write_enc(&env, "2", "account2@example.com", "live-rt");
    write_enc(&env, "1", "account1@example.com", "rt-1");

    env.run(["move", "1", "5"]).assert_code(0);
    assert_eq!(read_creds(&env, "5", "account1@example.com"), "rt-1");

    chmod(&enc, 0o000);
    let out = env.run(["move", "2", "6"]);
    chmod(&enc, 0o600);

    out.assert_code(1);
    assert!(out.stderr.contains("could not be read"), "{}", out.stderr);
    let data = sequence(&env);
    assert_eq!(accounts_email(&data, "2"), "account2@example.com");
    assert!(data["accounts"].get("6").is_none());
    assert_eq!(read_creds(&env, "2", "account2@example.com"), "live-rt");
}

#[test]
fn test_absent_source_still_moves() {
    let env = seeded(sample_sequence_data());
    env.run(["move", "2", "5"]).assert_code(0);
    assert_eq!(accounts_email(&sequence(&env), "5"), "account2@example.com");
}

#[test]
fn move_argument_errors_match_argparse() {
    let env = TestEnv::new();
    let p = prog();
    let usage = format!("usage: {p} move [-h] [--debug] NUM|EMAIL|ALIAS SLOT\n");
    let out = env.run(["move", "1"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!("{usage}{p} move: error: the following arguments are required: SLOT\n")
    );
    let out = env.run(["move", "1", "2", "--", "--", "3"]);
    assert_eq!(
        out.stderr,
        format!("{usage}{p} move: error: unrecognized arguments: -- 3\n")
    );
    let out = env.run(["move", "-h"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .contains("  SLOT             Destination slot number\n")
    );
}

#[test]
fn move_store_written_by_one_implementation_reads_back_in_the_other() {
    if !support::is_upstream() {
        return;
    }
    use support::Impl;
    let env = seeded(sample_sequence_data_with_org());
    write_creds(&env, "1", SAME_EMAIL, "creds-org");
    env.cmd(["move", "1", "4"])
        .using(Impl::Ccswap)
        .run()
        .assert_code(0);
    env.cmd(["swap", "4", "2"])
        .using(Impl::Upstream)
        .run()
        .assert_code(0);
    assert_eq!(read_creds(&env, "2", SAME_EMAIL), "creds-org");
    assert_eq!(
        sequence(&env)["accounts"]["2"]["organizationUuid"],
        "org-uuid-5678"
    );
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.bytes().filter(|c| *c != b'=') {
        let Some(v) = ALPHABET.iter().position(|a| *a == c) else {
            return String::new();
        };
        bits = (bits << 6) | v as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8(out).unwrap_or_default()
}
