//! Ported from upstream tests/test_switcher.py::TestProvenanceGuard,
//! TestSelfSwitchProvenance, TestDirectActivationPreservation,
//! TestUuidConflictClassification, TestSwitchToSelfSlotAndForce (force),
//! TestPerformSwitchPostDisplay (direct activation), test_json_output.py::
//! TestSwitchJson (force, from_unmanaged), test_transfer.py::
//! TestCleanHomeActivation (the switch after an import) and test_cli.py
//! (`unclaimed`).

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn oauth(access: &str, refresh: &str) -> Value {
    json!({"claudeAiOauth": {
        "accessToken": access,
        "refreshToken": refresh,
        "expiresAt": 9_999_999_999_000_i64,
    }})
}

fn creds(tag: &str) -> String {
    oauth(&format!("at-{tag}"), &format!("rt-{tag}")).to_string()
}

fn email(num: u32) -> String {
    ["", "a@example.com", "b@example.com", "c@example.com"][num as usize].to_string()
}

fn backup_account(num: u32) -> String {
    format!("account-{num}-{}", email(num))
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}")})
}

fn record(num: u32) -> Value {
    json!({"email": email(num), "uuid": format!("uuid-{num}"), "organizationUuid": "",
           "organizationName": "", "added": "2024-01-01T00:00:00Z"})
}

fn seed(env: &TestEnv, nums: &[u32], active: u32) {
    let accounts: serde_json::Map<String, Value> =
        nums.iter().map(|n| (n.to_string(), record(*n))).collect();
    env.seed_sequence(json!({
        "activeAccountNumber": active,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": nums,
        "accounts": accounts,
    }));
    for n in nums {
        env.security()
            .set_item(BACKUP_SERVICE, &backup_account(*n), &creds(&n.to_string()));
        env.write_file(
            env.store_dir()
                .join(format!("configs/.claude-config-{n}-{}.json", email(*n))),
            &json!({"oauthAccount": oauth_account(*n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(active), "numStartups": 3}),
    );
    set_live(env, &creds(&active.to_string()));
}

fn set_live(env: &TestEnv, value: &str) {
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, value);
}

fn live(env: &TestEnv) -> Option<String> {
    env.security().item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)
}

fn backup(env: &TestEnv, num: u32) -> Option<String> {
    env.security().item(BACKUP_SERVICE, &backup_account(num))
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn resolves_to(env: &TestEnv, uuid: &str, email: Option<&str>, org: Option<&str>) {
    let mut body = json!({"account": {"uuid": uuid, "email": email}});
    if let Some(org) = org {
        body["organization"] = json!({"uuid": org});
    }
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &body.to_string()),
    );
}

fn profile_requests(env: &TestEnv) -> usize {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/oauth/profile")
        .count()
}

fn credentials_dir(env: &TestEnv) -> PathBuf {
    env.store_dir().join("credentials")
}

/// `(id, base64 bytes)` of every stashed entry file.
fn stash_files(env: &TestEnv) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = std::fs::read_dir(credentials_dir(env))
        .map(|dir| {
            dir.filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().into_owned();
                let id = name.strip_prefix(".unclaimed-")?.strip_suffix(".enc")?;
                let text = std::fs::read_to_string(credentials_dir(env).join(&name)).unwrap();
                Some((id.to_string(), text))
            })
            .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn manifest(env: &TestEnv) -> Value {
    env.read_json(credentials_dir(env).join(".unclaimed-manifest.json"))
}

fn only_stash_entry(env: &TestEnv, expected_bytes: &str) -> (String, Value) {
    let files = stash_files(env);
    assert_eq!(files.len(), 1, "{files:?}");
    let (id, encoded) = &files[0];
    assert_eq!(encoded, &base64(expected_bytes.as_bytes()));
    let row = manifest(env)["entries"][id].clone();
    (id.clone(), row)
}

fn warnings(report: &Value) -> Vec<String> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
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

// test_access_token_rotation_same_lineage_backs_up
#[test]
fn a_same_lineage_rotation_is_backed_up_without_asking_the_network() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-1-new", "rt-1").to_string();
    set_live(&env, &rotated);

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(profile_requests(&env), 0);
    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    assert_eq!(warnings(&report), Vec::<String>::new());
}

// test_full_rotation_resolved_to_outgoing_slot_backs_up
#[test]
fn a_full_rotation_resolved_to_the_outgoing_slot_is_backed_up() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-1-new", "rt-1-rotated").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-1", Some("a@example.com"), Some(""));

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(profile_requests(&env), 1);
    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    assert_eq!(warnings(&report), Vec::<String>::new());
    assert!(stash_files(&env).is_empty());
}

// test_resolution_backfills_empty_slot_uuid
#[test]
fn a_resolved_rotation_backfills_an_empty_slot_uuid() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut seq = env.sequence();
    seq["accounts"]["1"]["uuid"] = json!("");
    env.seed_sequence(seq);
    set_live(&env, &oauth("at-f", "rt-1-rotated").to_string());
    resolves_to(&env, "uuid-resolved", Some("a@example.com"), Some(""));

    env.run(["switch", "2"]).assert_code(0);

    assert_eq!(env.sequence()["accounts"]["1"]["uuid"], "uuid-resolved");
}

// test_foreign_credential_preserved_never_backed_into_any_slot
#[test]
fn another_slots_credential_is_stashed_and_never_written_into_a_slot() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let foreign = oauth("at-2-rotated", "rt-2-rotated").to_string();
    set_live(&env, &foreign);
    resolves_to(&env, "uuid-2", Some("b@example.com"), Some(""));

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(0);
    let report = out.json();

    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(backup(&env, 2), Some(creds("2")));
    assert_eq!(
        warnings(&report),
        vec![
            "Credential ownership mismatch detected. The live credential was preserved and was not written into Account-1. If Account-2 later cannot authenticate, log in as it and run: ccswap add --slot 2"
        ]
    );
    let (id, row) = only_stash_entry(&env, &foreign);
    let keys: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "createdAt",
            "reason",
            "configSlot",
            "fingerprint",
            "liveOauthAccount",
            "resolvedIdentity",
            "credentialsMtime"
        ]
    );
    assert_eq!(row["reason"], "foreign");
    assert_eq!(row["configSlot"], "1");
    assert_eq!(row["fingerprint"].as_str().unwrap().len(), 7 + 64, "{row}");
    assert!(row["fingerprint"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(row["liveOauthAccount"], oauth_account(1));
    assert_eq!(
        row["resolvedIdentity"],
        json!({"uuid": "uuid-2", "email": "b@example.com", "organizationUuid": ""})
    );
    assert_eq!(row["credentialsMtime"], Value::Null);
    let created = row["createdAt"].as_str().unwrap();
    assert_eq!(created.len(), "2024-01-01T00:00:00Z".len(), "{created}");
    let parts: Vec<&str> = id.split('-').collect();
    assert_eq!(parts.len(), 3, "{id}");
    assert_eq!(parts[0].len(), "20240101T000000".len());
    assert_eq!(
        parts[0],
        created.replace(['-', ':'], "").trim_end_matches('Z')
    );
    assert_eq!(parts[1].len(), 12);
    assert_eq!(parts[2].len(), 6);
    assert_eq!(manifest(&env)["schemaVersion"], 1);
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(credentials_dir(&env).join(format!(".unclaimed-{id}.enc"))),
        0o600
    );
    assert_eq!(
        mode(credentials_dir(&env).join(".unclaimed-manifest.json")),
        0o600
    );
    assert_eq!(
        parse(&live(&env).unwrap()),
        parse(&creds("2")),
        "the switch proceeds onto the stored backup"
    );
}

// test_foreign_synced_lineage_warns_without_any_write
#[test]
fn another_slots_already_synced_credential_is_only_warned_about() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_live(&env, &creds("2"));
    resolves_to(&env, "uuid-2", Some("b@example.com"), Some(""));

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(backup(&env, 2), Some(creds("2")));
    assert!(stash_files(&env).is_empty());
    assert_eq!(
        warnings(&report),
        vec![
            "Credential ownership mismatch detected. The live credential already matches Account-2's stored backup, so nothing was written into Account-1."
        ]
    );
}

// test_alien_credential_preserved_and_skipped
#[test]
fn an_unmanaged_accounts_credential_is_stashed_with_a_human_warning() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let alien = oauth("at-x", "rt-x").to_string();
    set_live(&env, &alien);
    resolves_to(
        &env,
        "uuid-unmanaged",
        Some("elsewhere@example.com"),
        Some(""),
    );

    let out = env.run(["switch", "2"]);
    out.assert_code(0);

    assert!(
        out.stdout.starts_with(
            "The live login does not match a managed account. It was preserved and not written into Account-1. If you need that account, log in as it and run: ccswap add\nSwitched to Account-2 (b@example.com)\n"
        ),
        "{}",
        out.stdout
    );
    assert_eq!(backup(&env, 1), Some(creds("1")));
    let (_, row) = only_stash_entry(&env, &alien);
    assert_eq!(row["reason"], "alien");
}

// test_blank_stored_uuid_email_match_is_alien_not_foreign
#[test]
fn an_email_match_on_a_slot_without_a_uuid_is_alien() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut seq = env.sequence();
    seq["accounts"]["2"]["uuid"] = json!("");
    env.seed_sequence(seq);
    let drifted = oauth("at-d", "rt-d").to_string();
    set_live(&env, &drifted);
    resolves_to(&env, "uuid-2-real", Some("b@example.com"), Some(""));

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(backup(&env, 2), Some(creds("2")));
    let (_, row) = only_stash_entry(&env, &drifted);
    assert_eq!(row["reason"], "alien");
    assert!(warnings(&report)[0].contains("does not match a managed account"));
}

// test_email_match_with_conflicting_uuid_is_not_the_slot
#[test]
fn a_recycled_email_with_another_uuid_is_not_the_outgoing_slot() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-x", "rt-x").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-recycled-email", Some("a@example.com"), Some(""));

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1), Some(creds("1")));
    only_stash_entry(&env, &rotated);
    assert!(warnings(&report)[0].contains("does not match a managed account"));
}

// test_partial_identity_uuid_only_matching_outgoing_slot_backs_up
#[test]
fn a_uuid_only_identity_of_the_outgoing_slot_is_backed_up() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-f", "rt-1-rotated").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-1", None, None);

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    assert!(stash_files(&env).is_empty());
    assert_eq!(warnings(&report), Vec::<String>::new());
}

// test_partial_identity_uuid_match_with_slot_org_recorded
#[test]
fn a_uuid_only_identity_matches_a_slot_that_records_an_org() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mut seq = env.sequence();
    seq["accounts"]["1"]["organizationUuid"] = json!("org-1");
    env.seed_sequence(seq);
    let mut account = oauth_account(1);
    account["organizationUuid"] = json!("org-1");
    env.write_json(env.global_config_path(), &json!({"oauthAccount": account}));
    let rotated = oauth("at-f", "rt-1-rotated").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-1", None, None);

    env.run(["switch", "2"]).assert_code(0);

    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    assert!(stash_files(&env).is_empty());
}

// test_partial_identity_matching_nothing_falls_open
#[test]
fn a_partial_identity_matching_nothing_falls_open_to_the_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let mystery = oauth("at-x", "rt-x").to_string();
    set_live(&env, &mystery);
    resolves_to(&env, "uuid-nobody", None, None);

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1).as_deref(), Some(mystery.as_str()));
    assert!(stash_files(&env).is_empty());
    assert_eq!(warnings(&report), Vec::<String>::new());
}

// test_foreign_attribution_survives_missing_email
#[test]
fn a_uuid_and_org_match_on_another_slot_is_foreign_without_an_email() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let foreign = oauth("at-2-rotated", "rt-2-rotated").to_string();
    set_live(&env, &foreign);
    resolves_to(&env, "uuid-2", None, Some(""));

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(backup(&env, 2), Some(creds("2")));
    let (_, row) = only_stash_entry(&env, &foreign);
    assert_eq!(row["reason"], "foreign");
    assert!(warnings(&report)[0].contains("Account-2"));
}

// test_unresolvable_mismatch_backs_up_pre_fix / test_profile_exception_falls_back_to_pre_fix_backup
#[test]
fn an_unresolvable_divergence_is_backed_up_quietly() {
    for status in [500, 401] {
        let env = TestEnv::new();
        seed(&env, &[1, 2], 1);
        let mystery = oauth("at-x", "rt-x").to_string();
        set_live(&env, &mystery);
        env.http().respond(
            "GET",
            "/api/oauth/profile",
            StubResponse::json(status, "{}"),
        );

        let report = env.run(["switch", "2", "--json"]).json();

        assert_eq!(profile_requests(&env), 1);
        assert_eq!(backup(&env, 1).as_deref(), Some(mystery.as_str()));
        assert!(stash_files(&env).is_empty());
        assert_eq!(warnings(&report), Vec::<String>::new());
        assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
    }
}

// test_safety_copy_failure_aborts_before_live_overwrite
#[test]
fn a_failed_safety_copy_aborts_before_the_live_login_is_replaced() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let alien = oauth("at-x", "rt-x").to_string();
    set_live(&env, &alien);
    resolves_to(
        &env,
        "uuid-unmanaged",
        Some("elsewhere@example.com"),
        Some(""),
    );
    let dir = credentials_dir(&env);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = env.run(["switch", "2"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    out.assert_code(1);
    assert_eq!(live(&env).as_deref(), Some(alien.as_str()));
    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(env.sequence()["activeAccountNumber"], 1);
}

// test_moved_bytes_between_prefetch_and_lock_fall_to_unresolved
#[test]
fn a_resolution_of_bytes_that_moved_before_the_lock_is_discarded() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    set_live(&env, &oauth("at-before", "rt-before").to_string());
    let moved = oauth("at-m", "rt-m").to_string();
    let security = env.security().clone();
    let moved_live = moved.clone();
    env.http()
        .respond_with("GET", "/api/oauth/profile", move |_| {
            security.set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &moved_live);
            StubResponse::json(
                200,
                &json!({"account": {"uuid": "uuid-2", "email": "b@example.com"},
                        "organization": {"uuid": ""}})
                .to_string(),
            )
        });

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(backup(&env, 1).as_deref(), Some(moved.as_str()));
    assert!(stash_files(&env).is_empty());
    assert_eq!(warnings(&report), Vec::<String>::new());
}

// test_diverged_unresolvable_self_switch_noops_silently
#[test]
fn a_diverged_self_switch_whose_owner_is_unknown_changes_nothing() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let diverged = oauth("at-1-new", "rt-1-rotated").to_string();
    set_live(&env, &diverged);

    let report = env.run(["switch", "1", "--json"]).json();

    assert_eq!(profile_requests(&env), 1);
    assert_eq!(report["reason"], "already-active");
    assert_eq!(warnings(&report), Vec::<String>::new());
    assert_eq!(live(&env).as_deref(), Some(diverged.as_str()));
    assert_eq!(backup(&env, 1), Some(creds("1")));
}

// test_diverged_resolved_self_switch_reconciles
#[test]
fn a_diverged_self_switch_resolved_to_the_slot_resyncs_its_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let rotated = oauth("at-1-new", "rt-1-rotated").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-1", Some("a@example.com"), Some(""));

    let report = env.run(["switch", "1", "--json"]).json();

    assert_eq!(profile_requests(&env), 1, "resolved once, before the lock");
    assert_eq!(backup(&env, 1).as_deref(), Some(rotated.as_str()));
    let here = json!({"number": 1, "email": "a@example.com"});
    assert_eq!(report["switched"], false);
    assert_eq!(report["reason"], "already-active");
    assert_eq!(report["from"], here);
    assert_eq!(report["to"], here);
}

#[test]
fn a_rotation_landing_on_a_diverged_resolved_live_slot_reconciles() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 2);
    let mut seq = env.sequence();
    seq["activeAccountNumber"] = json!(1);
    env.seed_sequence(seq);
    let rotated = oauth("at-2-new", "rt-2-rotated").to_string();
    set_live(&env, &rotated);
    resolves_to(&env, "uuid-2", Some("b@example.com"), Some(""));

    let report = env.run(["switch", "--json"]).json();

    assert_eq!(report["reason"], "already-active");
    assert_eq!(backup(&env, 2).as_deref(), Some(rotated.as_str()));
    assert_eq!(env.sequence()["activeAccountNumber"], 2);
}

// test_switch_to_from_unmanaged_account / test_unmanaged_live_login_stashed_before_activation
#[test]
fn switching_from_an_unmanaged_login_stashes_it_and_activates_the_target() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.seed_login("stranger@example.com", "", "");
    let stranger = oauth("at-s", "rt-s").to_string();
    set_live(&env, &stranger);

    let out = env.run(["switch", "2", "--json"]);
    out.assert_code(0);
    let report = out.json();

    assert_eq!(report["switched"], true);
    assert_eq!(report["reason"], "switched");
    assert_eq!(
        report["from"],
        json!({"number": null, "email": "stranger@example.com"})
    );
    assert_eq!(report["to"], json!({"number": 2, "email": "b@example.com"}));
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
    assert_eq!(
        env.read_json(env.global_config_path())["oauthAccount"],
        oauth_account(2)
    );
    let (_, row) = only_stash_entry(&env, &stranger);
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(row["configSlot"], "unmanaged");
    assert_eq!(row["resolvedIdentity"], Value::Null);
    assert_eq!(
        row["liveOauthAccount"]["emailAddress"],
        "stranger@example.com"
    );
    assert_eq!(env.sequence()["activeAccountNumber"], 2);
    assert_eq!(backup(&env, 1), Some(creds("1")));
}

#[test]
fn a_human_direct_activation_says_activated() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.seed_login("stranger@example.com", "", "");

    let out = env.run(["switch", "2"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Activated Account-2 (b@example.com)\n\n"),
        "{}",
        out.stdout
    );
}

// test_stash_failure_aborts_direct_activation
#[test]
fn a_failed_safety_copy_aborts_direct_activation() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.seed_login("stranger@example.com", "", "");
    let stranger = oauth("at-s", "rt-s").to_string();
    set_live(&env, &stranger);
    let dir = credentials_dir(&env);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = env.run(["switch", "2", "--json"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    out.assert_code(1);
    let error = &out.json()["error"];
    assert_eq!(error["type"], "SwitchError");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.starts_with(
            "Could not preserve the live credential before activation (safety-copy write failed: "
        ) && message.ends_with("); aborting rather than destroying it"),
        "{message}"
    );
    assert_eq!(live(&env).as_deref(), Some(stranger.as_str()));
    assert_eq!(env.sequence()["activeAccountNumber"], 1);
}

// test_force_proceeds_with_warning_when_stash_fails
#[test]
fn force_proceeds_with_a_warning_when_the_safety_copy_fails() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let dir = credentials_dir(&env);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = env.run(["switch", "2", "--force", "--json"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    out.assert_code(0);
    let report = out.json();
    let warning = &warnings(&report)[0];
    assert!(
        warning.starts_with(
            "Could not preserve the replaced live credential (safety-copy write failed: "
        ) && warning
            .ends_with(") — proceeding because --force explicitly rewrites the live login."),
        "{warning}"
    );
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
}

// test_switch_to_force_self_activation_reports_activated / test_force_self_activation_restores_imported_creds
#[test]
fn force_onto_the_live_slot_rewrites_the_live_login_from_its_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let stale = oauth("at-stale", "rt-stale").to_string();
    set_live(&env, &stale);

    let out = env.run(["switch", "1", "--force", "--json"]);
    out.assert_code(0);
    let here = json!({"number": 1, "email": "a@example.com"});
    assert_eq!(
        out.json(),
        json!({
            "schemaVersion": 1, "switched": false, "from": here, "to": here,
            "strategy": "direct", "reason": "activated",
            "message": "Activated Account-1 (a@example.com) from stored backup",
            "warnings": [],
        })
    );
    assert_eq!(
        profile_requests(&env),
        0,
        "force never resolves the live login"
    );
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("1")));
    assert_eq!(
        backup(&env, 1),
        Some(creds("1")),
        "no backup of the stale login"
    );
    let (_, row) = only_stash_entry(&env, &stale);
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(row["configSlot"], "1");

    set_live(&env, &creds("1"));
    let out = env.run(["switch", "1", "--force"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Activated Account-1 (a@example.com)\n"),
        "{}",
        out.stdout
    );
    assert_eq!(
        stash_files(&env).len(),
        1,
        "the target's own bytes are not stashed"
    );
}

// test_switch_to_force_cross_slot_reports_switched / test_force_cross_slot_skips_backup_of_current
#[test]
fn force_onto_another_slot_skips_the_outgoing_backup() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    let stale = oauth("at-stale", "rt-1").to_string();
    set_live(&env, &stale);

    let report = env.run(["switch", "2", "--force", "--json"]).json();

    assert_eq!(report["switched"], true);
    assert_eq!(report["reason"], "switched");
    assert_eq!(
        report["from"],
        json!({"number": 1, "email": "a@example.com"})
    );
    assert_eq!(backup(&env, 1), Some(creds("1")));
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
    assert_eq!(env.sequence()["activeAccountNumber"], 2);
}

// test_direct_activation_rolls_back_live_creds_on_sequence_write_failure
#[test]
fn a_failed_direct_activation_restores_the_live_login_and_config() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.seed_login("stranger@example.com", "", "");
    let stranger = oauth("at-s", "rt-s").to_string();
    set_live(&env, &stranger);
    let config_before = std::fs::read_to_string(env.global_config_path()).unwrap();
    let store = env.store_dir();
    env.write_file(store.join(".lock"), "");
    env.write_file(store.join("claude-swap.log"), "");
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o500)).unwrap();

    let out = env.run(["switch", "2"]);
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();

    out.assert_code(1);
    assert_eq!(live(&env).as_deref(), Some(stranger.as_str()));
    assert_eq!(
        std::fs::read_to_string(env.global_config_path()).unwrap(),
        config_before
    );
    assert_eq!(env.sequence()["activeAccountNumber"], 1);
}

// test_unreadable_live_credentials_without_config_identity_abort
#[test]
fn an_unreadable_live_credential_aborts_direct_activation() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    std::fs::remove_file(env.global_config_path()).unwrap();
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, "");
    let file = env.write_file(env.home().join(".claude/.credentials.json"), &creds("x"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = env.run(["switch", "2", "--json"]);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    out.assert_code(1);
    assert_eq!(
        out.json()["error"],
        json!({"type": "CredentialReadError",
               "message": "Cannot snapshot live credentials before activation"})
    );
    assert!(!env.global_config_path().exists());
}

// test_orphaned_live_login_without_config_identity_is_stashed / test_switch_rotate_after_import_uses_active_from_envelope
#[test]
fn a_bare_switch_without_a_live_identity_activates_the_recorded_slot() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 2);
    std::fs::remove_file(env.global_config_path()).unwrap();
    let orphaned = oauth("at-o", "rt-o").to_string();
    set_live(&env, &orphaned);

    let out = env.run(["switch", "--json"]);
    out.assert_code(0);
    let report = out.json();

    assert_eq!(report["switched"], true);
    assert_eq!(report["from"], Value::Null);
    assert_eq!(report["to"], json!({"number": 2, "email": "b@example.com"}));
    assert_eq!(report["strategy"], "rotation");
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("2")));
    assert_eq!(
        env.read_json(env.global_config_path()),
        json!({"oauthAccount": oauth_account(2)})
    );
    let (_, row) = only_stash_entry(&env, &orphaned);
    assert_eq!(row["reason"], "displaced-live-login");
    assert_eq!(row["configSlot"], "2");
    assert_eq!(row["liveOauthAccount"], Value::Null);
}

// test_switch_to_after_import_activates_target
#[test]
fn a_fresh_machine_with_no_live_login_stashes_nothing() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    std::fs::remove_file(env.global_config_path()).unwrap();
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, "");

    let out = env.run(["switch"]);
    out.assert_code(0);
    assert!(
        out.stdout
            .starts_with("Activated Account-1 (a@example.com)\n"),
        "{}",
        out.stdout
    );
    assert!(stash_files(&env).is_empty());
    assert_eq!(parse(&live(&env).unwrap()), parse(&creds("1")));
}

#[test]
fn a_fresh_machine_skips_a_broken_recorded_slot() {
    let env = TestEnv::new();
    seed(&env, &[1, 2, 3], 2);
    std::fs::remove_file(env.global_config_path()).unwrap();
    let mut seq = env.sequence();
    seq["accounts"]["1"]["disabled"] = json!(true);
    env.seed_sequence(seq);
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(2), "");

    let out = env.run(["switch"]);
    out.assert_code(0);
    assert!(
        out.stdout.starts_with(
            "Skipping Account-2 (no stored credentials/config, re-add with ccswap --add-account --slot 2)\nActivated Account-3 (c@example.com)\n"
        ),
        "{}",
        out.stdout
    );

    std::fs::remove_file(env.global_config_path()).unwrap();
    let mut seq = env.sequence();
    seq["activeAccountNumber"] = json!(1);
    env.seed_sequence(seq);
    let report = env.run(["switch", "--json"]).json();
    assert_eq!(warnings(&report), vec!["Skipped Account-1 (disabled)"]);
    assert_eq!(report["to"]["number"], 3);
}

#[test]
fn a_fresh_machine_with_no_usable_slot_fails() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    std::fs::remove_file(env.global_config_path()).unwrap();
    let mut seq = env.sequence();
    seq["accounts"]["1"]["disabled"] = json!(true);
    env.seed_sequence(seq);
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(2), "");

    let out = env.run(["switch", "--json"]);
    out.assert_code(1);
    assert_eq!(
        out.json()["error"],
        json!({"type": "ConfigError", "message": "No accounts remain in rotation. Re-enable one with: ccswap enable <num|email>"})
    );

    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(1), "");
    let out = env.run(["switch", "--json"]);
    out.assert_code(1);
    assert_eq!(
        out.json()["error"]["message"],
        "No managed accounts have valid stored credentials/config. Re-add a slot with: ccswap --add-account --slot <number>"
    );
}

// test_clean_switch_preserves_existing_local_config / test_a_valid_empty_config_is_spliced_not_called_unparseable
#[test]
fn a_fresh_machine_keeps_local_settings_and_splices_an_empty_config() {
    for local in [
        json!({"tipsHistory": {"shown": ["welcome"]}, "numStartups": 42}),
        json!({}),
    ] {
        let env = TestEnv::new();
        seed(&env, &[1, 2], 1);
        env.write_json(env.global_config_path(), &local);

        env.run(["switch", "2"]).assert_code(0);

        let mut expected = local.clone();
        expected["oauthAccount"] = oauth_account(2);
        assert_eq!(env.read_json(env.global_config_path()), expected);
        let salvaged = std::fs::read_dir(env.home())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".unreadable-"));
        assert!(!salvaged);
    }
}

// test_clean_switch_fallback_when_local_config_malformed
#[test]
fn a_fresh_machine_replaces_a_malformed_config_after_keeping_a_copy() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    env.write_file(env.global_config_path(), "{not valid json");

    let report = env.run(["switch", "2", "--json"]).json();

    assert_eq!(
        env.read_json(env.global_config_path()),
        json!({"oauthAccount": oauth_account(2)})
    );
    assert!(
        warnings(&report)[0].starts_with(
            ".claude.json could not be parsed — a copy was kept at .claude.json.unreadable-"
        ),
        "{report}"
    );
}

// test_direct_activation_without_config_identity_composes_live_state
#[test]
fn a_fresh_machine_keeps_the_live_machine_shared_credential_state() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    std::fs::remove_file(env.global_config_path()).unwrap();
    let mut target = oauth("at-1", "rt-1");
    target["mcpOAuth"] = json!({"server": {"refreshToken": "stale"}});
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(1), &target.to_string());
    set_live(
        &env,
        &json!({"mcpOAuth": {"server": {"refreshToken": "current"}}}).to_string(),
    );

    env.run(["switch", "1"]).assert_code(0);

    let mut expected = oauth("at-1", "rt-1");
    expected["mcpOAuth"] = json!({"server": {"refreshToken": "current"}});
    assert_eq!(parse(&live(&env).unwrap()), expected);
}

#[test]
fn unclaimed_lists_and_purges_stashed_entries() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);

    let out = env.run(["unclaimed"]);
    out.assert_code(0);
    assert_eq!(out.stdout, "No unclaimed credential entries\n");

    let alien = oauth("at-x", "rt-x").to_string();
    set_live(&env, &alien);
    resolves_to(
        &env,
        "uuid-unmanaged",
        Some("elsewhere@example.com"),
        Some(""),
    );
    env.run(["switch", "2"]).assert_code(0);
    let (id, _) = only_stash_entry(&env, &alien);
    env.write_file(credentials_dir(&env).join(".unclaimed-0orphan.enc"), "eA==");

    let out = env.run(["unclaimed"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("0orphan  slot ?  orphaned (no manifest row)\n{id}  slot 1  alien\n")
    );

    let out = env.run(["unclaimed", "--purge", &id]);
    out.assert_code(0);
    assert_eq!(out.stdout, format!("Purged {id}\n"));
    assert!(
        !credentials_dir(&env)
            .join(format!(".unclaimed-{id}.enc"))
            .exists()
    );
    assert_eq!(manifest(&env)["entries"], json!({}));

    let out = env.run(["unclaimed", "--purge", &id]);
    out.assert_code(1);
    assert_eq!(out.stderr, format!("Error: no unclaimed entry {id}\n"));
}

#[test]
fn unclaimed_parses_its_own_arguments() {
    let env = TestEnv::new();
    seed(&env, &[1], 1);
    let prog = support::PROG;

    let out = env.run(["unclaimed", "--help"]);
    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "usage: {prog} claude unclaimed [-h] [--purge ID] [--debug]\n\nList stashed credential entries, or purge one by id. Purging deletes the bytes\n— recovery is /login + `{prog} add`.\n\noptions:\n  -h, --help  show this help message and exit\n  --purge ID  Delete this entry's bytes and manifest row\n  --debug     Enable debug logging\n"
        )
    );

    let out = env.run(["unclaimed", "extra"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!(
            "usage: {prog} unclaimed [-h] [--purge ID] [--debug]\n{prog} unclaimed: error: unrecognized arguments: extra\n"
        )
    );

    let out = env.run(["unclaimed", "--purge"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .ends_with("error: argument --purge: expected one argument\n"),
        "{}",
        out.stderr
    );

    let out = env.run(["unclaimed", "--pur=nope", "--deb"]);
    out.assert_code(1);
    assert_eq!(out.stderr, "Error: no unclaimed entry nope\n");
}

// _replan_new_active runs after a direct activation too
#[test]
fn a_direct_activation_pulls_the_new_active_plan_to_the_floor() {
    let env = TestEnv::new();
    seed(&env, &[1, 2], 1);
    std::fs::remove_file(env.global_config_path()).unwrap();
    let mut seq = env.sequence();
    seq["activeAccountNumber"] = json!(2);
    env.seed_sequence(seq);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let fetched = now - 30.0;
    env.write_json(
        ".claude-swap-backup/cache/usage.json",
        &json!({"schemaVersion": 2, "accounts": {"2": {
            "email": "b@example.com", "organizationUuid": "",
            "lastGood": {"five_hour": {"pct": 33}}, "fetchedAt": fetched,
            "nextPollAt": now + 3600.0, "pollIntervalS": 3600.0,
        }}}),
    );

    env.run(["switch"]).assert_code(0);

    let row = &env.read_json(".claude-swap-backup/cache/usage.json")["accounts"]["2"];
    assert!(
        row["nextPollAt"].as_f64().unwrap() <= fetched + 180.0 + 1.0,
        "{row}"
    );
}
