//! Ported from upstream tests/test_switcher.py: TestConsumeGate,
//! TestInactiveRefreshRoutesThroughGate, TestConsumeGateLockFailures,
//! TestPermanentlyUnreadableStashRow, TestGateUltraReviewFixes,
//! TestStashReaderUnreadableVsAbsent. A test whose upstream form hands the
//! gate a stale snapshot is driven through the 401 path instead, with the usage
//! handler changing the store while the slot's fetch is in flight.

mod support;

use support::hint;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ccswap_core::anthropic_client::credential_fingerprint;
use ccswap_core::env::Environment;
use ccswap_core::paths::Paths;
use ccswap_core::session_profile::{self, STALE_MARKER};
use ccswap_core::unclaimed::UnclaimedStash;
use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubResponse, TestEnv};

const A: &str = "a@example.com";
const B: &str = "b@example.com";
const BACKUP_SERVICE: &str = "claude-swap";
const ITEM: &str = "account-2-b@example.com";

fn creds(access: &str, refresh: &str, expires_at: i64) -> String {
    json!({"claudeAiOauth": {"accessToken": access, "refreshToken": refresh,
                             "expiresAt": expires_at}})
    .to_string()
}

fn old() -> String {
    creds("sk-old", "rt-old", 1000)
}

fn new() -> String {
    creds("sk-new", "rt-new", 9_999_999_999_000)
}

/// Not expired locally, so the usage pass sends it and only a 401 reaches
/// the gate.
fn live_looking(access: &str, refresh: &str) -> String {
    creds(access, refresh, 9_999_999_999_000)
}

fn setup(backup: &str) -> TestEnv {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2],
        "accounts": {
            "1": {"email": A, "uuid": "u1", "organizationUuid": "", "organizationName": "",
                  "added": "2024-01-01T00:00:00Z"},
            "2": {"email": B, "uuid": "u2", "organizationUuid": "", "organizationName": "",
                  "added": "2024-01-02T00:00:00Z"}
        }
    }));
    env.seed_login(A, "", "");
    let live = live_looking("tok-a", "rt-a");
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &live);
    env.security()
        .set_item(BACKUP_SERVICE, &format!("account-1-{A}"), &live);
    if !backup.is_empty() {
        env.security().set_item(BACKUP_SERVICE, ITEM, backup);
    }
    usage_answers(&env, |_| None);
    env
}

/// The server rejects `sk-old`; `side` runs first and may answer instead.
fn usage_answers<F>(env: &TestEnv, side: F)
where
    F: Fn(&str) -> Option<StubResponse> + Send + Sync + 'static,
{
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            let bearer = req
                .header("Authorization")
                .unwrap_or_default()
                .trim_start_matches("Bearer ")
                .to_string();
            if let Some(response) = side(&bearer) {
                return response;
            }
            if bearer == "sk-old" {
                return StubResponse::json(401, "{}");
            }
            StubResponse::json(200, r#"{"five_hour": {"utilization": 5}}"#)
        });
}

type Posted = Arc<Mutex<Vec<String>>>;

fn token_endpoint<F>(env: &TestEnv, answer: F) -> Posted
where
    F: Fn(&str) -> StubResponse + Send + Sync + 'static,
{
    let posted: Posted = Arc::default();
    let log = posted.clone();
    env.http()
        .respond_with("POST", "/v1/oauth/token", move |req| {
            let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
            let rt = body["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            log.lock().unwrap().push(rt.clone());
            answer(&rt)
        });
    posted
}

fn rotated(access: &str, refresh: &str) -> StubResponse {
    StubResponse::json(
        200,
        &json!({"access_token": access, "refresh_token": refresh, "expires_in": 3600}).to_string(),
    )
}

fn invalid_grant() -> StubResponse {
    StubResponse::json(400, r#"{"error": "invalid_grant"}"#)
}

fn posts(posted: &Posted) -> Vec<String> {
    posted.lock().unwrap().clone()
}

fn list(env: &TestEnv) -> Value {
    let out = env.run(["list", "--json"]);
    out.assert_code(0);
    out.json()
}

fn slot2(payload: &Value) -> Value {
    payload["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["number"] == 2)
        .cloned()
        .unwrap()
}

fn backup(env: &TestEnv) -> Option<String> {
    env.security().item(BACKUP_SERVICE, ITEM)
}

fn oauth_field(credentials: &str, key: &str) -> Value {
    serde_json::from_str::<Value>(credentials).unwrap()["claudeAiOauth"][key].clone()
}

fn last_error(env: &TestEnv) -> Value {
    env.read_json(".claude-swap-backup/cache/usage.json")["accounts"]["2"]["lastError"].clone()
}

fn paths(env: &TestEnv) -> Paths {
    Paths::new(&Environment::from_pairs([(
        "HOME",
        env.home().display().to_string(),
    )]))
}

fn credentials_dir(env: &TestEnv) -> PathBuf {
    env.store_dir().join("credentials")
}

fn manifest(env: &TestEnv) -> PathBuf {
    credentials_dir(env).join(".unclaimed-manifest.json")
}

fn entry_path(env: &TestEnv, id: &str) -> PathBuf {
    credentials_dir(env).join(format!(".unclaimed-{id}.enc"))
}

fn stash_row(env: &TestEnv, credentials: &str, row: Value) -> String {
    let Value::Object(row) = row else {
        unreachable!()
    };
    UnclaimedStash::new(&paths(env))
        .write(credentials, row)
        .unwrap()
}

/// A prior gate consumed `consumed`'s grant and parked `successor`.
fn stash_successor_of(env: &TestEnv, successor: &str, consumed: &str) -> String {
    stash_row(
        env,
        successor,
        json!({
            "reason": "consume-gate-persist-lock-failed",
            "configSlot": "2",
            "consumedFp": credential_fingerprint(consumed),
            "fingerprint": credential_fingerprint(successor),
        }),
    )
}

/// `(id, reason)` of every stash row.
fn stash(env: &TestEnv) -> Vec<(String, Value)> {
    UnclaimedStash::new(&paths(env))
        .list()
        .into_iter()
        .map(|(id, row)| {
            let reason = row.get("reason").cloned().unwrap_or(Value::Null);
            (id, reason)
        })
        .collect()
}

fn session_dir(env: &TestEnv) -> PathBuf {
    let dir = session_profile::session_dir_for(&paths(env), "2", B);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Held {
    _file: std::fs::File,
}

fn hold(path: PathBuf) -> Held {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) },
        0,
        "flock {}",
        path.display()
    );
    Held { _file: file }
}

fn chmod(path: &PathBuf, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn inactive_refresh_routes_through_gate_test_expired_inactive_fetch_uses_gate() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    let payload = list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert!(
        credentials_dir(&env).join(".consume-2.lock").exists(),
        "the refresh went through the per-slot consume lock"
    );
    let stored = backup(&env).unwrap();
    assert_eq!(oauth_field(&stored, "refreshToken"), "rt-new");
    assert_eq!(slot2(&payload)["usageStatus"], "ok");
}

#[test]
fn consume_gate_test_gate_rereads_under_lock_and_posts_rereread_bytes() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    let fresher = creds("sk-fresher", "rt-fresher", 2000);
    let security = env.security().clone();
    let swapped = fresher.clone();
    usage_answers(&env, move |bearer| {
        (bearer == "sk-old").then(|| {
            security.set_item(BACKUP_SERVICE, ITEM, &swapped);
            StubResponse::json(401, "{}")
        })
    });
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-fresher"],
        "re-read bytes, not the snapshot"
    );
    assert_eq!(
        oauth_field(&backup(&env).unwrap(), "refreshToken"),
        "rt-new"
    );
}

#[test]
fn consume_gate_test_gate_cas_persist_detects_racing_writer() {
    let env = setup(&old());
    let racer = creds("sk-racer", "rt-racer", 8_888_888_888_000);
    let security = env.security().clone();
    let landed = racer.clone();
    let posted = token_endpoint(&env, move |_| {
        security.set_item(BACKUP_SERVICE, ITEM, &landed);
        rotated("sk-new", "rt-new")
    });

    list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(backup(&env).as_deref(), Some(racer.as_str()));
    let rows = stash(&env);
    assert_eq!(
        rows.len(),
        1,
        "consumed successor must be stashed, never discarded"
    );
    assert_eq!(rows[0].1, "consume-gate-cas-conflict");
}

#[test]
fn consume_gate_test_gate_prefers_newer_session_profile_lineage() {
    let env = setup(&old());
    let profile_newer = creds("sk-prof", "rt-prof", 5000);
    std::fs::write(session_dir(&env).join(".credentials.json"), &profile_newer).unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(posts(&posted), ["rt-prof"]);
}

#[test]
fn consume_gate_test_gate_invalid_grant_returns_error_without_persist() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(last_error(&env), "invalid_grant");
    assert_eq!(backup(&env), Some(old()), "the dead bytes stay put");
}

#[test]
fn consume_gate_test_an_unreadable_stash_manifest_defers_instead_of_posting() {
    let env = setup(&old());
    stash_successor_of(&env, &new(), &old());
    chmod(&manifest(&env), 0o000);
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);
    chmod(&manifest(&env), 0o600);

    assert!(
        posts(&posted).is_empty(),
        "POSTed the spent generation while its successor sat in a stash the gate could not see"
    );
    assert_eq!(last_error(&env), "stash-unreadable");
}

#[test]
fn consume_gate_test_the_purge_exit_the_fail_closed_message_names_actually_works() {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    std::fs::write(manifest(&env), "{not json at all").unwrap();

    let listed = env.run(["unclaimed"]);
    listed.assert_code(0);
    assert!(
        listed.stdout.contains(&id),
        "corrupt manifest hid the orphan from `unclaimed`: {}",
        listed.stdout
    );
    env.run(["unclaimed", "--purge", &id]).assert_code(0);
    assert!(!entry_path(&env, &id).exists());

    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));
    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-old"],
        "the documented exit did not unblock it"
    );
    assert_eq!(
        oauth_field(&backup(&env).unwrap(), "refreshToken"),
        "rt-new"
    );
}

#[test]
fn consume_gate_test_an_unlistable_dir_counts_as_entries_at_risk() {
    let env = setup(&old());
    stash_successor_of(&env, &new(), &old());
    std::fs::write(manifest(&env), "{not json at all").unwrap();
    // Pre-create the consume lock: an unlistable dir still lets files be
    // opened, but not every implementation creates the lock the same way.
    std::fs::write(credentials_dir(&env).join(".consume-2.lock"), "").unwrap();
    let posted = token_endpoint(&env, |_| invalid_grant());

    chmod(&credentials_dir(&env), 0o311);
    let out = env.run(["list", "--json"]);
    chmod(&credentials_dir(&env), 0o700);
    out.assert_code(0);

    assert!(
        posts(&posted).is_empty(),
        "an unknowable scan read as nothing at risk"
    );
    assert_eq!(last_error(&env), "stash-unreadable");
}

#[test]
fn consume_gate_test_a_manifest_without_a_dict_entries_member_is_corrupt() {
    for payload in [
        r#"{"schemaVersion": 1, "entries": "bogus"}"#,
        r#"{"schemaVersion": 1}"#,
    ] {
        let env = setup(&old());
        stash_successor_of(&env, &new(), &old());
        std::fs::write(manifest(&env), payload).unwrap();
        let posted = token_endpoint(&env, |_| invalid_grant());

        list(&env);

        assert!(
            posts(&posted).is_empty(),
            "{payload}: read as ok-with-no-rows"
        );
        assert_eq!(last_error(&env), "stash-unreadable", "{payload}");
    }
}

#[test]
fn consume_gate_test_a_corrupt_manifest_with_orphan_entries_fails_closed() {
    let env = setup(&old());
    stash_successor_of(&env, &new(), &old());
    std::fs::write(manifest(&env), "{not json at all").unwrap();
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert!(
        posts(&posted).is_empty(),
        "POSTed a spent grant with a successor on disk"
    );
    assert_eq!(last_error(&env), "stash-unreadable");
}

#[test]
fn consume_gate_test_a_corrupt_manifest_still_posts_rather_than_deadlocking() {
    let env = setup(&old());
    std::fs::write(manifest(&env), "{not json at all").unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    let payload = list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-old"],
        "a corrupt manifest froze the slot"
    );
    assert_eq!(slot2(&payload)["usageStatus"], "ok");
}

#[test]
fn consume_gate_test_a_manifest_of_invalid_utf8_is_corrupt_not_unreadable() {
    let env = setup(&old());
    std::fs::write(manifest(&env), b"\xff\xfe\x00not utf8").unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-old"],
        "undecodable bytes with nothing stashed must read as corrupt (proceed), not unreadable (defer)"
    );
}

#[test]
fn consume_gate_test_a_readable_empty_manifest_still_posts() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(
        oauth_field(&backup(&env).unwrap(), "refreshToken"),
        "rt-new"
    );
}

/// The slot's credential disappears while its fetch is in flight, so the
/// gate's re-read finds it absent.
fn remove_slot_during_fetch(env: &TestEnv) {
    let security = env.security().clone();
    usage_answers(env, move |bearer| {
        (bearer == "sk-old").then(|| {
            security.fail_command_for_item("find-generic-password", BACKUP_SERVICE, ITEM, 44);
            StubResponse::json(401, "{}")
        })
    });
}

#[test]
fn consume_gate_test_a_removed_slot_defers_instead_of_posting_the_snapshot() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    remove_slot_during_fetch(&env);
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert!(
        posts(&posted).is_empty(),
        "spent a grant for a slot that no longer exists"
    );
    assert!(
        stash(&env).is_empty(),
        "stashed a successor for an account the user deleted"
    );
}

#[test]
fn consume_gate_test_an_absent_slot_defers_even_when_a_newer_profile_exists() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    std::fs::write(
        session_dir(&env).join(".credentials.json"),
        creds("sk-prof", "rt-prof", 9_999_999_999_500),
    )
    .unwrap();
    // The profile's own token is served read-only; reject it so the backup
    // path (and its 401) is what runs.
    let security = env.security().clone();
    usage_answers(&env, move |bearer| match bearer {
        "sk-prof" => Some(StubResponse::json(500, "{}")),
        "sk-old" => {
            security.fail_command_for_item("find-generic-password", BACKUP_SERVICE, ITEM, 44);
            Some(StubResponse::json(401, "{}"))
        }
        _ => None,
    });
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert!(
        posts(&posted).is_empty(),
        "healed a removed slot from its surviving profile"
    );
    assert_eq!(
        backup(&env).as_deref(),
        Some(live_looking("sk-old", "rt-old").as_str()),
        "resurrected the slot's stored credential from the profile"
    );
}

#[test]
fn consume_gate_lock_failures_test_lock_failure_before_post_is_transient() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    let held = hold(env.store_dir().join(".lock"));
    let out = env.run(["list", "--json"]);
    drop(held);
    out.assert_code(0);

    assert!(posts(&posted).is_empty(), "nothing consumed");
    assert_eq!(backup(&env), Some(old()));
}

#[test]
fn consume_gate_lock_failures_test_a_stashed_successor_is_not_reported_as_freshened() {
    let env = setup(&old());
    let lock = env.store_dir().join(".lock");
    let posted = token_endpoint(&env, move |_| {
        // Hold the slot lock across the gate's post-POST CAS window.
        let held = hold(lock.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(13));
            drop(held);
        });
        rotated("sk-new", "rt-new")
    });

    let payload = list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(
        backup(&env),
        Some(old()),
        "the store still holds the spent generation"
    );
    let rows = stash(&env);
    assert_eq!(rows.len(), 1, "successor must be stashed");
    assert_eq!(rows[0].1, "consume-gate-persist-lock-failed");
    assert_eq!(
        slot2(&payload)["usageStatus"],
        "ok",
        "the successor still serves this pass's request"
    );
}

#[test]
fn permanently_unreadable_stash_row_test_ten_passes_name_the_condition_instead_of_network() {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    chmod(&entry_path(&env, &id), 0o000);
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);
    chmod(&entry_path(&env, &id), 0o600);

    assert!(
        posts(&posted).is_empty(),
        "the spent generation must never be POSTed"
    );
    assert_eq!(last_error(&env), "stash-unreadable");
    assert!(
        stash(&env).iter().any(|(i, _)| *i == id),
        "the row must survive"
    );
}

#[test]
fn permanently_unreadable_stash_row_test_the_kind_carries_its_remedy_and_skips_the_doomed_fetch() {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    chmod(&entry_path(&env, &id), 0o000);
    token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    let out = env.run(["list"]);
    chmod(&entry_path(&env, &id), 0o600);
    out.assert_code(0);

    assert!(
        out.stdout.contains(&hint("cswap unclaimed")),
        "{}",
        out.stdout
    );
    assert!(
        !env.http()
            .requests()
            .iter()
            .any(|r| r.path() == "/api/oauth/usage"
                && r.header("Authorization") == Some("Bearer sk-old")),
        "spent a guaranteed 401 on a token known to be expired"
    );
}

#[test]
fn gate_ultra_review_fixes_test_a_cas_conflict_is_not_reported_as_a_failed_freshen() {
    let env = setup(&old());
    let racer = creds("sk-rac", "rt-rac", 8_888_888_888_000);
    let security = env.security().clone();
    let landed = racer.clone();
    token_endpoint(&env, move |_| {
        security.set_item(BACKUP_SERVICE, ITEM, &landed);
        rotated("sk-new", "rt-new")
    });

    let payload = list(&env);

    assert_eq!(backup(&env).as_deref(), Some(racer.as_str()));
    assert_eq!(slot2(&payload)["usageStatus"], "ok");
    assert!(
        env.http()
            .requests()
            .iter()
            .any(|r| r.path() == "/api/oauth/usage"
                && r.header("Authorization") == Some("Bearer sk-rac")),
        "the racer's newer lineage is what the pass adopted"
    );
}

#[test]
fn gate_ultra_review_fixes_test_a_cas_conflict_stash_does_not_accumulate_forever() {
    let env = setup(&old());
    let third = creds("sk-3rd", "rt-3rd", 2000);
    let security = env.security().clone();
    let landed = third.clone();
    let posted = token_endpoint(&env, move |rt| {
        if rt == "rt-old" {
            security.set_item(BACKUP_SERVICE, ITEM, &landed);
        }
        rotated("sk-new", "rt-new")
    });

    list(&env);
    assert_eq!(
        stash(&env).len(),
        1,
        "the consumed successor must be preserved"
    );
    std::fs::remove_file(env.store_dir().join("cache/usage.json")).unwrap();
    list(&env);

    assert_eq!(posts(&posted), ["rt-old", "rt-3rd"]);
    assert!(
        !stash(&env)
            .iter()
            .any(|(_, reason)| reason == "consume-gate-cas-conflict"),
        "a CAS-conflict entry stays pending forever"
    );
}

#[test]
fn gate_ultra_review_fixes_test_contention_reports_its_own_kind_not_transient() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    let held = hold(credentials_dir(&env).join(".consume-2.lock"));
    let out = env.run(["list", "--json"]);
    drop(held);
    out.assert_code(0);

    assert!(posts(&posted).is_empty());
    assert_eq!(last_error(&env), "consume-busy");
}

#[test]
fn gate_ultra_review_fixes_test_persist_and_stash_both_failing_still_never_raises() {
    let env = setup(&old());
    let security = env.security().clone();
    let dir = credentials_dir(&env);
    let posted = token_endpoint(&env, move |_| {
        security.fail_command("add-generic-password", 51);
        chmod(&dir, 0o500);
        rotated("sk-new", "rt-new")
    });

    let out = env.run(["list", "--json"]);
    chmod(&credentials_dir(&env), 0o700);
    out.assert_code(0);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(backup(&env), Some(old()));
    assert!(stash(&env).is_empty());
    assert_eq!(slot2(&out.json())["usageStatus"], "ok");
}

#[test]
fn gate_ultra_review_fixes_test_failure_outcome_carries_consumed_fp_of_posted_bytes() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    let fresher = creds("sk-fresher", "rt-fresher", 2000);
    let security = env.security().clone();
    let swapped = fresher.clone();
    usage_answers(&env, move |bearer| {
        (bearer == "sk-old").then(|| {
            security.set_item(BACKUP_SERVICE, ITEM, &swapped);
            StubResponse::json(401, "{}")
        })
    });
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert_eq!(posts(&posted), ["rt-fresher"]);
    let row = &env.read_json(".claude-swap-backup/cache/usage.json")["accounts"]["2"];
    assert_eq!(row["lastError"], "invalid_grant");
    assert_eq!(
        row["struckFingerprint"].as_str(),
        credential_fingerprint(&fresher).as_deref(),
        "the strike binds to the POSTed bytes, not the snapshot"
    );
}

#[test]
fn gate_ultra_review_fixes_test_concurrent_gates_consume_only_one_grant() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| {
        std::thread::sleep(Duration::from_millis(800));
        rotated("sk-new", "rt-new")
    });

    std::thread::scope(|s| {
        let a = s.spawn(|| env.run(["list", "--json"]));
        let b = s.spawn(|| env.run(["list", "--json"]));
        a.join().unwrap().assert_code(0);
        b.join().unwrap().assert_code(0);
    });

    assert_eq!(
        posts(&posted),
        ["rt-old"],
        "one grant consumed exactly once"
    );
    assert_eq!(
        oauth_field(&backup(&env).unwrap(), "refreshToken"),
        "rt-new"
    );
}

#[test]
fn gate_ultra_review_fixes_test_next_gate_pass_adopts_stashed_successor() {
    let env = setup(&old());
    stash_successor_of(&env, &new(), &old());
    let posted = token_endpoint(&env, |_| rotated("sk-x", "rt-x"));

    let payload = list(&env);

    assert!(posts(&posted).is_empty(), "no second grant consumed");
    assert_eq!(backup(&env), Some(new()));
    assert!(stash(&env).is_empty(), "stash entry consumed");
    assert_eq!(slot2(&payload)["usageStatus"], "ok");
}

#[test]
fn gate_ultra_review_fixes_test_slot_removed_mid_post_stashes_instead_of_resurrecting() {
    let env = setup(&old());
    let security = env.security().clone();
    token_endpoint(&env, move |_| {
        security.fail_command_for_item("find-generic-password", BACKUP_SERVICE, ITEM, 44);
        rotated("sk-new", "rt-new")
    });

    list(&env);

    assert_eq!(
        backup(&env),
        Some(old()),
        "removed slot must not get the successor"
    );
    let rows = stash(&env);
    assert_eq!(rows.len(), 1, "successor parked in stash");
    assert_eq!(rows[0].1, "consume-gate-slot-removed");
}

#[test]
fn gate_ultra_review_fixes_test_stale_marked_profile_never_supersedes_backup() {
    let reimported = creds("sk-readd", "rt-readd", 1000);
    let env = setup(&reimported);
    let dir = session_dir(&env);
    std::fs::write(
        dir.join(".credentials.json"),
        creds("sk-stale", "rt-stale", 999_999),
    )
    .unwrap();
    std::fs::write(dir.join(STALE_MARKER), "").unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-readd"],
        "the re-added backup, not the presumed-stale profile"
    );
}

#[test]
fn gate_ultra_review_fixes_test_a_foreign_profile_never_supersedes_the_backup() {
    let env = setup(&creds("sk-bk", "rt-bk", 1000));
    let dir = session_dir(&env);
    std::fs::write(
        dir.join(".credentials.json"),
        creds("sk-foreign", "rt-foreign", 999_999),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"oauthAccount": {"emailAddress": "SOMEONE-ELSE@example.com",
               "accountUuid": "other-uuid", "organizationUuid": "",
               "organizationName": ""}})
        .to_string(),
    )
    .unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-bk"],
        "the gate POSTed a profile logged in as another account"
    );
    assert!(!backup(&env).unwrap().contains("rt-foreign"));
}

#[test]
fn gate_ultra_review_fixes_test_an_older_profile_never_supersedes_the_backup() {
    let env = setup(&creds("sk-bk", "rt-bk", 5000));
    std::fs::write(
        session_dir(&env).join(".credentials.json"),
        creds("sk-pf", "rt-pf-SPENT", 1000),
    )
    .unwrap();
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert_eq!(posts(&posted), ["rt-bk"]);
}

#[test]
fn gate_ultra_review_fixes_test_unreadable_backup_defers_instead_of_posting_snapshot() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    let security = env.security().clone();
    usage_answers(&env, move |bearer| {
        (bearer == "sk-old").then(|| {
            security.fail_command_for_item("find-generic-password", BACKUP_SERVICE, ITEM, 51);
            StubResponse::json(401, "{}")
        })
    });
    let posted = token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    assert!(posts(&posted).is_empty());
    assert_eq!(last_error(&env), "refresh-failed");
}

#[test]
fn gate_ultra_review_fixes_test_fresh_rotated_reread_adopted_without_second_consume() {
    let env = setup(&live_looking("sk-old", "rt-old"));
    let security = env.security().clone();
    usage_answers(&env, move |bearer| {
        (bearer == "sk-old").then(|| {
            security.set_item(BACKUP_SERVICE, ITEM, &new());
            StubResponse::json(401, "{}")
        })
    });
    let posted = token_endpoint(&env, |_| rotated("sk-x", "rt-x"));

    let payload = list(&env);

    assert!(posts(&posted).is_empty());
    assert_eq!(slot2(&payload)["usageStatus"], "ok");
}

fn byteless_non_matching_row(bytes_survive: bool) {
    let current = creds("sk-cur", "rt-cur", 1000);
    let env = setup(&current);
    let id = stash_successor_of(&env, &new(), &old());
    if !bytes_survive {
        std::fs::remove_file(entry_path(&env, &id)).unwrap();
    }
    token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    list(&env);

    let listed = stash(&env).iter().any(|(i, _)| *i == id);
    if bytes_survive {
        assert!(
            listed,
            "a superseded row that still holds its bytes is a real credential"
        );
    } else {
        assert!(!listed, "a row whose bytes are gone can never be adopted");
    }
}

#[test]
fn gate_ultra_review_fixes_test_a_byteless_non_matching_row_is_retired_bytes_survive() {
    byteless_non_matching_row(true);
}

#[test]
fn gate_ultra_review_fixes_test_a_byteless_non_matching_row_is_retired_bytes_gone() {
    byteless_non_matching_row(false);
}

fn unreadable_non_matching_row(row_a_readable: bool) {
    let env = setup(&old());
    let row_a = stash_row(
        &env,
        &new(),
        json!({"reason": "consume-gate-persist-failed", "configSlot": "2",
               "consumedFp": "sha256:some-other-generation"}),
    );
    stash_successor_of(&env, &new(), &old());
    if !row_a_readable {
        chmod(&entry_path(&env, &row_a), 0o000);
    }
    let posted = token_endpoint(&env, |_| rotated("sk-x", "rt-x"));

    list(&env);
    chmod(&entry_path(&env, &row_a), 0o600);

    assert!(
        posts(&posted).is_empty(),
        "the adoptable row behind it was not reached"
    );
    assert_eq!(backup(&env), Some(new()));
    assert!(
        stash(&env).iter().any(|(i, _)| *i == row_a),
        "a row whose bytes are merely unreadable still holds a real credential"
    );
}

#[test]
fn gate_ultra_review_fixes_test_an_unreadable_non_matching_row_does_not_abort_the_scan_readable() {
    unreadable_non_matching_row(true);
}

#[test]
fn gate_ultra_review_fixes_test_an_unreadable_non_matching_row_does_not_abort_the_scan_unreadable()
{
    unreadable_non_matching_row(false);
}

fn denied_session_dir(denied: Option<&str>) {
    let env = setup(&old());
    let dir = session_dir(&env);
    std::fs::write(dir.join(".credentials.json"), old()).unwrap();
    let mut chmodded = Vec::new();
    if denied.is_some() {
        chmodded.push(dir.clone());
    }
    if denied == Some("session_dir_and_parent") {
        chmodded.push(dir.parent().unwrap().to_path_buf());
    }
    token_endpoint(&env, |_| rotated("sk-new", "rt-new"));

    for d in &chmodded {
        chmod(d, 0o500);
    }
    let out = env.run(["list", "--json"]);
    for d in chmodded.iter().rev() {
        chmod(d, 0o700);
    }
    out.assert_code(0);

    assert_eq!(
        oauth_field(&backup(&env).unwrap(), "refreshToken"),
        "rt-new",
        "premise: the store write advanced the slot"
    );
    let stale = session_profile::is_stale(&dir);
    match denied {
        None => {
            assert!(
                !dir.join(".credentials.json").exists(),
                "the invalidation ran"
            );
            assert!(!stale);
        }
        Some("session_dir") => {
            assert!(dir.join(".credentials.json").exists());
            assert!(
                stale,
                "the invalidation was denied and no stale marker landed"
            );
        }
        Some(_) => {
            assert!(dir.join(".credentials.json").exists());
            assert!(!stale);
            let log = std::fs::read_to_string(env.store_dir().join("claude-swap.log"))
                .unwrap_or_default();
            assert!(
                log.lines()
                    .any(|l| l.contains("ERROR") && l.contains("account 2")),
                "nothing says so above WARNING:\n{log}"
            );
        }
    }
}

#[test]
fn gate_ultra_review_fixes_test_a_denied_session_dir_still_leaves_the_stale_marker_control() {
    denied_session_dir(None);
}

#[test]
fn gate_ultra_review_fixes_test_a_denied_session_dir_still_leaves_the_stale_marker_session_dir() {
    denied_session_dir(Some("session_dir"));
}

#[test]
fn gate_ultra_review_fixes_test_a_denied_session_dir_still_leaves_the_stale_marker_and_parent() {
    denied_session_dir(Some("session_dir_and_parent"));
}

#[test]
fn stash_reader_unreadable_vs_absent_test_row_a_unreadable_entry_does_not_post_the_spent_generation()
 {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    chmod(&entry_path(&env, &id), 0o000);
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);
    chmod(&entry_path(&env, &id), 0o600);

    assert!(posts(&posted).is_empty());
    assert_eq!(last_error(&env), "stash-unreadable");
    assert!(
        !stash(&env).is_empty(),
        "the entry must survive for the next pass"
    );
}

#[test]
fn stash_reader_unreadable_vs_absent_test_row_b_control_readable_entry_adopts() {
    let env = setup(&old());
    stash_successor_of(&env, &new(), &old());
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert!(
        posts(&posted).is_empty(),
        "adopted successor must short-circuit the POST"
    );
    assert_eq!(backup(&env), Some(new()));
    assert!(stash(&env).is_empty(), "stash entry consumed");
}

#[test]
fn stash_reader_unreadable_vs_absent_test_row_c_control_nothing_stashed_posts_the_snapshot() {
    let env = setup(&old());
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert_eq!(posts(&posted), ["rt-old"]);
    assert_eq!(last_error(&env), "invalid_grant");
}

#[test]
fn stash_reader_unreadable_vs_absent_test_corrupt_entry_keeps_its_current_behaviour() {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    std::fs::write(entry_path(&env, &id), "not-valid-base64!!!").unwrap();
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert_eq!(
        posts(&posted),
        ["rt-old"],
        "a corrupt entry must behave like absent"
    );
    assert_eq!(last_error(&env), "invalid_grant");
    assert_eq!(backup(&env), Some(old()));
}

#[test]
fn stash_reader_unreadable_vs_absent_test_row_d_absent_entry_bytes_terminate_instead_of_deferring()
{
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    std::fs::remove_file(entry_path(&env, &id)).unwrap();
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert_eq!(posts(&posted), ["rt-old"], "an absent entry must not defer");
    assert_eq!(last_error(&env), "invalid_grant");
}

#[test]
fn stash_reader_unreadable_vs_absent_test_absent_entry_is_retired_not_rescanned_forever() {
    let env = setup(&old());
    let id = stash_successor_of(&env, &new(), &old());
    std::fs::remove_file(entry_path(&env, &id)).unwrap();
    token_endpoint(&env, |_| invalid_grant());

    list(&env);

    assert!(
        stash(&env).is_empty(),
        "an absent-bytes stash entry must be retired"
    );
}

#[test]
fn stash_reader_unreadable_vs_absent_test_unreadable_row_does_not_starve_a_readable_sibling() {
    let env = setup(&old());
    let unreadable = stash_successor_of(&env, &new(), &old());
    let second = stash_successor_of(&env, &new(), &old());
    chmod(&entry_path(&env, &unreadable), 0o000);
    let posted = token_endpoint(&env, |_| invalid_grant());

    list(&env);
    chmod(&entry_path(&env, &unreadable), 0o600);

    assert!(
        posts(&posted).is_empty(),
        "the readable sibling must be adopted"
    );
    assert_eq!(backup(&env), Some(new()));
    assert!(!stash(&env).iter().any(|(i, _)| *i == second));
}
