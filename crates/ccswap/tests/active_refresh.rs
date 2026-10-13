//! The active slot's usage fetch: an expired or server-rejected active
//! token is refreshed under Claude Code's lock protocol, and only a lineage
//! attributed to the slot is consumed or written into its backup.
//!
//! Ported from upstream test_switcher.py::TestActiveAccountRefresh,
//! TestActiveRefreshProvenance and TestActiveSlotStrikeParity, driven
//! through `list` / `list --json`. One process is one usage pass, so the
//! cases that need the live store to change between the pass's read and
//! the locked re-read, or an in-process verdict seeded by hand, are not
//! ported (see the list at the end of this file).

mod support;

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, StubRequest, StubResponse, TestEnv};

const EMAIL: &str = "test@example.com";
const BACKUP_SERVICE: &str = "claude-swap";
const FAR: i64 = 9_999_999_999_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn creds(access: &str, refresh: Option<&str>, expires_at: Option<i64>) -> String {
    let mut oauth = json!({"accessToken": access});
    if let Some(r) = refresh {
        oauth["refreshToken"] = json!(r);
    }
    if let Some(e) = expires_at {
        oauth["expiresAt"] = json!(e);
    }
    json!({"claudeAiOauth": oauth}).to_string()
}

fn expired() -> String {
    creds("sk-active", Some("rt-orig"), Some(1000))
}

fn backup_account() -> String {
    format!("account-1-{EMAIL}")
}

/// Slot 1 (`test@example.com`, uuid `uuid-1`, personal) is the live login.
fn env_with(uuid: Option<&str>, live: &str, backup: &str) -> TestEnv {
    let env = TestEnv::new();
    let mut record = json!({"email": EMAIL, "organizationUuid": "", "organizationName": "",
                            "added": "2024-01-01T00:00:00Z"});
    if let Some(u) = uuid {
        record["uuid"] = json!(u);
    }
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1],
        "accounts": {"1": record}
    }));
    env.seed_login(EMAIL, "", "");
    if !live.is_empty() {
        env.security()
            .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, live);
    }
    if !backup.is_empty() {
        env.security()
            .set_item(BACKUP_SERVICE, &backup_account(), backup);
    }
    env
}

fn setup(live: &str, backup: &str) -> TestEnv {
    env_with(Some("uuid-1"), live, backup)
}

fn live(env: &TestEnv) -> Option<String> {
    env.security().item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT)
}

fn backup(env: &TestEnv) -> Option<String> {
    env.security().item(BACKUP_SERVICE, &backup_account())
}

fn oauth_field(credentials: &str, key: &str) -> Value {
    serde_json::from_str::<Value>(credentials).unwrap()["claudeAiOauth"][key].clone()
}

/// The token endpoint rotates any grant to `sk-new` / `rt-new`.
fn stub_refresh_ok(env: &TestEnv) {
    env.http().respond(
        "POST",
        "/v1/oauth/token",
        StubResponse::json(
            200,
            r#"{"access_token": "sk-new", "refresh_token": "rt-new", "expires_in": 3600}"#,
        ),
    );
}

/// Usage per bearer token; anything else is a 500.
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

fn ok_usage(pct: u32) -> StubResponse {
    StubResponse::json(
        200,
        &format!(r#"{{"five_hour": {{"utilization": {pct}}}}}"#),
    )
}

fn token_posts(env: &TestEnv) -> Vec<StubRequest> {
    env.http()
        .requests()
        .into_iter()
        .filter(|r| r.path() == "/v1/oauth/token")
        .collect()
}

fn posted_refresh_tokens(env: &TestEnv) -> Vec<String> {
    token_posts(env)
        .iter()
        .map(|r| {
            serde_json::from_str::<Value>(&r.body).unwrap()["refresh_token"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn usage_bearers(env: &TestEnv) -> Vec<String> {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/oauth/usage")
        .map(|r| r.header("Authorization").unwrap_or_default().to_string())
        .collect()
}

fn profile_probes(env: &TestEnv) -> usize {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/oauth/profile")
        .count()
}

fn listed(env: &TestEnv) -> Value {
    let out = env.run(["list", "--json"]);
    out.assert_code(0);
    out.json()["accounts"][0].clone()
}

fn cache_row(env: &TestEnv) -> Value {
    env.read_json(".claude-swap-backup/cache/usage.json")["accounts"]["1"].clone()
}

fn forget_usage_cache(env: &TestEnv) {
    let _ = std::fs::remove_file(env.store_dir().join("cache/usage.json"));
}

fn log_text(env: &TestEnv) -> String {
    std::fs::read_to_string(env.store_dir().join("claude-swap.log")).unwrap_or_default()
}

fn seq(env: &TestEnv) -> Value {
    env.read_json(".claude-swap-backup/sequence.json")
}

fn oauth_refresh_lock(env: &TestEnv) -> PathBuf {
    env.claude_config_home().join(".oauth_refresh.lock")
}

fn legacy_credentials_lock(env: &TestEnv) -> PathBuf {
    env.home().join(".claude.lock")
}

fn config_lock(env: &TestEnv) -> PathBuf {
    env.home().join(".claude.json.lock")
}

/// Holds an exclusive `flock` on `path` until dropped (another process's
/// `FileLock`).
struct Flock {
    _file: std::fs::File,
}

impl Flock {
    fn hold(path: PathBuf) -> Self {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        // SAFETY: flock on a descriptor we own.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        Flock { _file: file }
    }
}

// --- TestActiveAccountRefresh -------------------------------------------------

#[test]
fn active_account_refresh_test_expired_refreshes_under_locks_and_persists_both_stores() {
    let env = setup(&expired(), &expired());
    let held = Arc::new(Mutex::new(Vec::new()));
    let (seen, locks) = (
        held.clone(),
        [
            oauth_refresh_lock(&env),
            legacy_credentials_lock(&env),
            config_lock(&env),
        ],
    );
    env.http()
        .respond_with("POST", "/v1/oauth/token", move |_| {
            seen.lock()
                .unwrap()
                .push(locks.iter().map(|l| l.is_dir()).collect::<Vec<_>>());
            StubResponse::json(
                200,
                r#"{"access_token": "sk-new", "refresh_token": "rt-new", "expires_in": 3600}"#,
            )
        });
    let released = Arc::new(Mutex::new(Vec::new()));
    let (seen, locks) = (
        released.clone(),
        [
            oauth_refresh_lock(&env),
            legacy_credentials_lock(&env),
            config_lock(&env),
        ],
    );
    env.http()
        .respond_with("GET", "/api/oauth/usage", move |req| {
            seen.lock().unwrap().push(locks.iter().any(|l| l.is_dir()));
            if req.header("Authorization") == Some("Bearer sk-new") {
                ok_usage(10)
            } else {
                StubResponse::json(500, "{}")
            }
        });

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 10.0);
    // Both credential locks held across the POST; the config lock is not.
    assert_eq!(*held.lock().unwrap(), [vec![true, true, false]]);
    // Every lock is released before the usage request.
    assert_eq!(*released.lock().unwrap(), [false]);
    assert_eq!(posted_refresh_tokens(&env), ["rt-orig"]);
    let live = live(&env).unwrap();
    assert_eq!(oauth_field(&live, "accessToken"), "sk-new");
    assert_eq!(oauth_field(&live, "refreshToken"), "rt-new");
    assert_eq!(backup(&env).unwrap(), live);
}

#[test]
fn active_account_refresh_test_owner_present_no_longer_blocks_the_refresh() {
    let env = setup(&expired(), &expired());
    let dir = env.store_dir().join("sessions/1-test_example.com");
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let pid = std::process::id();
    std::fs::write(
        dir.join("sessions").join(format!("{pid}.json")),
        json!({"pid": pid, "sessionId": "s", "cwd": "/w"}).to_string(),
    )
    .unwrap();
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(5))]);

    assert_eq!(listed(&env)["usageStatus"], "ok");
    assert_eq!(oauth_field(&live(&env).unwrap(), "accessToken"), "sk-new");
}

#[test]
fn active_account_refresh_test_a_held_consume_lock_defers_the_active_refresh() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    let _holder = Flock::hold(env.store_dir().join("credentials/.consume-1.lock"));

    let a = listed(&env);

    assert!(
        token_posts(&env).is_empty(),
        "POSTed a backup grant while another consume held its lock"
    );
    assert_eq!(a["usageStatus"], "token_expired");
}

#[test]
fn active_account_refresh_test_filelock_contention_defers_instead_of_raising() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    let _holder = Flock::hold(env.store_dir().join(".lock"));

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
    assert!(usage_bearers(&env).is_empty());
}

#[test]
fn active_account_refresh_test_unattributed_live_recovers_from_the_slots_backup() {
    let slot_backup = creds("sk-b", Some("rt-slot"), Some(1000));
    let env = setup(&expired(), &slot_backup);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(5))]);

    assert_eq!(listed(&env)["usageStatus"], "ok");
    assert_eq!(posted_refresh_tokens(&env), ["rt-slot"]);
    assert_eq!(oauth_field(&live(&env).unwrap(), "accessToken"), "sk-new");
}

#[test]
fn active_account_refresh_test_unattributed_live_with_unusable_backup_never_consumes() {
    let dead_backup = json!({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}}).to_string();
    let foreign_live = creds("sk-x", Some("rt-foreign"), Some(1000));
    let env = setup(&foreign_live, &dead_backup);
    stub_refresh_ok(&env);

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
    assert!(usage_bearers(&env).is_empty());
    assert!(
        log_text(&env).contains(
            "Active credential does not match Account-1's stored backup and the backup is unusable; cannot refresh (provenance unknown)."
        ),
        "{}",
        log_text(&env)
    );
}

#[test]
fn active_account_refresh_test_stranded_live_store_is_restored_from_the_backup() {
    let successor = creds("sk-successor", Some("rt-successor"), Some(FAR));
    let env = setup(&expired(), &successor);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-successor", ok_usage(5))]);

    assert_eq!(listed(&env)["usageStatus"], "ok");
    assert!(token_posts(&env).is_empty(), "restore, not a refresh");
    assert_eq!(live(&env).as_deref(), Some(successor.as_str()));
    assert_eq!(backup(&env).as_deref(), Some(successor.as_str()));
    assert_eq!(usage_bearers(&env), ["Bearer sk-successor"]);
}

#[test]
fn active_account_refresh_test_locally_valid_but_server_rejected_token_refreshes() {
    let revoked = creds("sk-revoked", Some("rt-orig"), Some(FAR));
    let env = setup(&revoked, &revoked);
    stub_refresh_ok(&env);
    stub_usage(
        &env,
        vec![
            ("sk-revoked", StubResponse::json(401, "{}")),
            ("sk-new", ok_usage(5)),
        ],
    );

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 5.0);
    assert_eq!(oauth_field(&live(&env).unwrap(), "accessToken"), "sk-new");
    assert_eq!(usage_bearers(&env), ["Bearer sk-revoked", "Bearer sk-new"]);
}

#[test]
fn active_account_refresh_test_server_rejected_token_with_no_recovery_surfaces_the_401() {
    let revoked = creds("sk-revoked", Some("rt-foreign"), Some(FAR));
    let dead_backup = json!({"claudeAiOauth": {"accessToken": "", "refreshToken": ""}}).to_string();
    let env = setup(&revoked, &dead_backup);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-revoked", StubResponse::json(401, "{}"))]);

    let a = listed(&env);

    assert_eq!(cache_row(&env)["lastError"], "http-401");
    assert_ne!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
}

#[test]
fn active_account_refresh_test_server_rejected_token_lock_contention_still_surfaces_the_401() {
    let revoked = creds("sk-revoked", Some("rt-orig"), Some(FAR));
    let env = setup(&revoked, &revoked);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-revoked", StubResponse::json(401, "{}"))]);
    std::fs::create_dir_all(oauth_refresh_lock(&env)).unwrap();

    let a = listed(&env);

    assert_eq!(cache_row(&env)["lastError"], "http-401");
    assert_ne!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
}

#[test]
fn active_account_refresh_test_no_refresh_token_is_permanent_not_transient() {
    let no_rt = creds("sk-only", None, Some(1000));
    let env = setup(&no_rt, &no_rt);
    stub_refresh_ok(&env);

    listed(&env);

    assert_eq!(cache_row(&env)["lastError"], "no_refresh_token");
    assert!(token_posts(&env).is_empty());
    assert!(usage_bearers(&env).is_empty());
}

#[test]
fn active_account_refresh_test_transient_refresh_failure_backs_off_via_the_store() {
    let env = setup(&expired(), &expired());
    env.http()
        .respond("POST", "/v1/oauth/token", StubResponse::json(500, "{}"));

    listed(&env);

    assert_eq!(cache_row(&env)["lastError"], "refresh-failed");
    assert!(usage_bearers(&env).is_empty());
    assert_eq!(live(&env), Some(expired()));
}

#[test]
fn active_account_refresh_test_lock_timeout_defers_to_the_holder() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    std::fs::create_dir_all(oauth_refresh_lock(&env)).unwrap();

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
    assert!(usage_bearers(&env).is_empty());
}

#[test]
fn active_account_refresh_test_consumed_generation_survives_a_live_write_failure() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(5))]);
    env.security()
        .fail_command_for_service("add-generic-password", ACTIVE_OAUTH_SERVICE, 45);
    // No plaintext fallback either: a directory sits where the file would go.
    std::fs::create_dir_all(env.claude_config_home().join(".credentials.json")).unwrap();

    let a = listed(&env);

    assert_eq!(posted_refresh_tokens(&env), ["rt-orig"]);
    assert_eq!(oauth_field(&backup(&env).unwrap(), "accessToken"), "sk-new");
    assert_eq!(a["usageStatus"], "token_expired");
    assert!(usage_bearers(&env).is_empty());
}

#[test]
fn active_account_refresh_test_non_expired_fetches_without_refreshing() {
    let fresh = creds("sk-live", Some("rt-live"), Some(FAR));
    let env = setup(&fresh, &fresh);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-live", ok_usage(3))]);

    assert_eq!(listed(&env)["usage"]["fiveHour"]["pct"], 3.0);
    assert!(token_posts(&env).is_empty());
}

fn refreshed() -> String {
    creds("sk-new", Some("rt-new"), Some(FAR))
}

fn profile(uuid: &str, email: Option<&str>, org: Option<&str>) -> String {
    let mut body = json!({"account": {"uuid": uuid}});
    if let Some(e) = email {
        body["account"]["email"] = json!(e);
    }
    if let Some(o) = org {
        body["organization"] = json!({"uuid": o});
    }
    body.to_string()
}

/// The live login was rotated elsewhere (`sk-new` / `rt-new`, fresh) while
/// the backup still holds the consumed predecessor.
fn drifted(uuid: Option<&str>) -> TestEnv {
    let env = env_with(uuid, &refreshed(), &expired());
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(3))]);
    env
}

#[test]
fn active_account_refresh_test_fresh_fetch_resyncs_backup_after_external_rotation() {
    let env = drifted(Some("uuid-1"));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-1", Some(EMAIL), None)),
    );

    let a = listed(&env);

    assert_eq!(a["usage"]["fiveHour"]["pct"], 3.0);
    assert!(
        token_posts(&env).is_empty(),
        "nothing consumed — pure resync"
    );
    assert_eq!(profile_probes(&env), 1);
    assert_eq!(backup(&env), Some(refreshed()));
}

#[test]
fn active_account_refresh_test_fresh_fetch_same_lineage_skips_the_resync() {
    let env = setup(&refreshed(), &refreshed());
    stub_usage(&env, vec![("sk-new", ok_usage(3))]);

    listed(&env);

    assert_eq!(profile_probes(&env), 0);
    assert_eq!(backup(&env), Some(refreshed()));
}

#[test]
fn active_account_refresh_test_fresh_foreign_probe_mismatch_skips_resync_warns_once_and_caches() {
    let env = drifted(Some("uuid-1"));
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &profile("uuid-foreign", Some("other@example.com"), None),
        ),
    );

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "foreign_credential");
    assert_eq!(backup(&env), Some(expired()));
    assert_eq!(profile_probes(&env), 1);
    assert_eq!(
        log_text(&env)
            .matches("resolves to a different account")
            .count(),
        1
    );
}

#[test]
fn active_account_refresh_test_fresh_probe_failure_skips_resync_and_retries_next_pass() {
    let env = drifted(Some("uuid-1"));
    env.http()
        .respond("GET", "/api/oauth/profile", StubResponse::json(500, "{}"));

    let first = listed(&env);

    assert_eq!(first["usage"]["fiveHour"]["pct"], 3.0);
    assert_eq!(backup(&env), Some(expired()));
    assert_eq!(profile_probes(&env), 1);

    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-1", Some(EMAIL), None)),
    );
    forget_usage_cache(&env);
    listed(&env);

    assert_eq!(profile_probes(&env), 2);
    assert_eq!(backup(&env), Some(refreshed()));
}

#[test]
fn active_account_refresh_test_fresh_probe_unverifiable_not_cached() {
    let env = drifted(None);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-x", None, None)),
    );

    listed(&env);
    forget_usage_cache(&env);
    listed(&env);

    assert_eq!(backup(&env), Some(expired()));
    assert_eq!(profile_probes(&env), 2);
}

#[test]
fn active_account_refresh_test_uuid_less_slot_email_match_with_missing_org_is_unverifiable() {
    let env = drifted(None);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-x", Some(EMAIL), None)),
    );

    let first = listed(&env);

    assert_eq!(backup(&env), Some(expired()));
    assert!(seq(&env)["accounts"]["1"].get("uuid").is_none());
    assert_eq!(first["usage"]["fiveHour"]["pct"], 3.0);
}

#[test]
fn active_account_refresh_test_uuid_less_personal_slot_rejects_same_email_under_foreign_org() {
    let env = drifted(None);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(
            200,
            &profile("uuid-org-sibling", Some(EMAIL), Some("org-B")),
        ),
    );

    let first = listed(&env);

    assert_eq!(backup(&env), Some(expired()));
    assert_eq!(first["usageStatus"], "foreign_credential");
    assert!(seq(&env)["accounts"]["1"].get("uuid").is_none());
}

#[test]
fn active_account_refresh_test_uuid_less_slot_exact_email_org_match_resyncs_and_backfills() {
    let env = drifted(None);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-resolved", Some(EMAIL), Some(""))),
    );

    let first = listed(&env);

    assert_eq!(first["usage"]["fiveHour"]["pct"], 3.0);
    assert_eq!(backup(&env), Some(refreshed()));
    assert_eq!(seq(&env)["accounts"]["1"]["uuid"], "uuid-resolved");
}

#[test]
fn active_account_refresh_test_expiry_after_unresynced_rotation_defers_without_verified_lineage() {
    let live_b = creds("sk-B", Some("rt-B"), Some(2000));
    let backup_a = creds("sk-A", Some("rt-A-consumed"), Some(1000));
    let env = setup(&live_b, &backup_a);
    stub_refresh_ok(&env);

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "token_expired");
    assert!(token_posts(&env).is_empty());
    assert_eq!(backup(&env), Some(backup_a));
    assert_eq!(live(&env), Some(live_b));
    assert!(usage_bearers(&env).is_empty());
}

#[test]
fn active_account_refresh_test_stranded_backup_newer_still_restores_not_consumes_live() {
    let successor = creds("sk-successor", Some("rt-successor"), Some(FAR));
    let env = setup(&expired(), &successor);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-successor", ok_usage(5))]);

    assert_eq!(listed(&env)["usageStatus"], "ok");
    assert!(token_posts(&env).is_empty());
    assert_eq!(live(&env), Some(successor));
}

#[test]
fn active_account_refresh_test_no_token_returns_no_credentials() {
    let no_token = json!({"claudeAiOauth": {"refreshToken": "rt-x"}}).to_string();
    let env = setup(&no_token, &expired());
    stub_refresh_ok(&env);

    assert_eq!(listed(&env)["usageStatus"], "no_credentials");
    assert!(usage_bearers(&env).is_empty());
    assert!(token_posts(&env).is_empty());
}

#[test]
fn active_account_refresh_test_list_renders_token_expired_line_on_lock_contention() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    std::fs::create_dir_all(oauth_refresh_lock(&env)).unwrap();

    let out = env.run(["list"]);

    out.assert_code(0);
    assert!(out.stdout.contains("token expired"), "{}", out.stdout);
    assert!(token_posts(&env).is_empty());
}

#[test]
fn active_account_refresh_test_expired_active_is_no_longer_statically_gated() {
    let env = setup(&expired(), &expired());
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(25))]);

    let a = listed(&env);

    assert_eq!(a["usageStatus"], "ok");
    assert_eq!(a["usage"]["fiveHour"]["pct"], 25.0);
}

#[test]
fn active_account_refresh_test_dead_lineage_surfaces_invalid_grant_not_a_silent_sentinel() {
    let env = setup(&expired(), &expired());
    env.http().respond(
        "POST",
        "/v1/oauth/token",
        StubResponse::json(400, r#"{"error": "invalid_grant"}"#),
    );

    listed(&env);

    let row = cache_row(&env);
    assert_eq!(row["lastError"], "invalid_grant");
    assert_eq!(row["authDeadStrikes"], 1);
    assert!(usage_bearers(&env).is_empty());
}

// --- TestActiveRefreshProvenance ----------------------------------------------

#[test]
fn active_refresh_provenance_test_unattributed_live_grant_is_never_consumed() {
    let live_creds = creds("sk-live", Some("rt-live"), Some(1000));
    let stored = creds("sk-stored", Some("rt-stored"), Some(1000));
    let env = setup(&live_creds, &stored);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(1))]);

    listed(&env);

    assert_eq!(posted_refresh_tokens(&env), ["rt-stored"]);
}

#[test]
fn active_refresh_provenance_test_same_lineage_live_credential_still_refreshes() {
    let live_creds = creds("sk-live", Some("rt-live"), Some(1000));
    let older = creds("sk-older", Some("rt-live"), None);
    let env = setup(&live_creds, &older);
    stub_refresh_ok(&env);
    stub_usage(&env, vec![("sk-new", ok_usage(10))]);

    let a = listed(&env);

    assert_eq!(a["usage"]["fiveHour"]["pct"], 10.0);
    assert_eq!(posted_refresh_tokens(&env), ["rt-live"]);
    assert_eq!(usage_bearers(&env), ["Bearer sk-new"]);
    let live = live(&env).unwrap();
    assert_eq!(oauth_field(&live, "accessToken"), "sk-new");
    assert_eq!(backup(&env), Some(live));
}

// --- TestActiveSlotStrikeParity -----------------------------------------------

fn seed_strike(env: &TestEnv, struck: &str) {
    env.write_json(
        ".claude-swap-backup/cache/usage.json",
        &json!({"schemaVersion": 2, "accounts": {"1": {
            "email": EMAIL, "organizationUuid": "",
            "lastAttemptAt": now_ms() as f64 / 1000.0,
            "consecutiveFailures": 1, "lastError": "invalid_grant",
            "authDeadStrikes": 1, "struckFingerprint": struck,
        }}}),
    );
}

/// `credential_fingerprint` of a credential whose refresh token is `rt-dead`.
const RT_DEAD_FP: &str = "sha256:b7ad5de3e61133c54672c432f03cbb16976a132dd2c90d6b23cdc584ad8241ef";

#[test]
fn active_slot_strike_parity_test_active_strike_bound_to_backup_not_healed_by_live_fp() {
    let dead_backup = creds("sk-dead", Some("rt-dead"), Some(1000));
    let live_creds = creds("sk-live", Some("rt-live"), Some(2000));
    let env = setup(&live_creds, &dead_backup);
    seed_strike(&env, RT_DEAD_FP);

    assert_eq!(listed(&env)["usageStatus"], "relogin_required");
    assert!(token_posts(&env).is_empty());
}

#[test]
fn active_slot_strike_parity_test_idle_slot_keeps_live_fp_heal_semantics() {
    let replaced = creds("sk-re", Some("rt-re"), Some(FAR));
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": null,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1],
        "accounts": {"1": {"email": EMAIL, "uuid": "uuid-1", "organizationUuid": "",
                           "organizationName": "", "added": "2024-01-01T00:00:00Z"}}
    }));
    env.security()
        .set_item(BACKUP_SERVICE, &backup_account(), &replaced);
    seed_strike(&env, "sha256:deadbeef");
    stub_usage(&env, vec![("sk-re", ok_usage(4))]);

    assert_ne!(listed(&env)["usageStatus"], "relogin_required");
}

#[test]
fn active_slot_strike_parity_test_active_path_refuses_consume_under_securestorage_env() {
    let env = setup("", &expired());
    std::fs::create_dir_all(env.claude_config_home()).unwrap();
    std::fs::write(
        env.claude_config_home().join(".credentials.json"),
        expired(),
    )
    .unwrap();
    stub_refresh_ok(&env);

    let redirected = env.root().join("redir");
    let out = env
        .cmd(["list", "--json"])
        .env(
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            redirected.to_str().unwrap(),
        )
        .run();
    out.assert_code(0);

    assert!(token_posts(&env).is_empty());
    assert_eq!(cache_row(&env)["lastError"], "store-unmirrored");
}

#[test]
fn active_slot_strike_parity_test_degraded_read_never_feeds_resync_write() {
    let env = setup("", &expired());
    std::fs::create_dir_all(env.claude_config_home()).unwrap();
    std::fs::write(
        env.claude_config_home().join(".credentials.json"),
        refreshed(),
    )
    .unwrap();
    env.security()
        .fail_command_for_service("find-generic-password", ACTIVE_OAUTH_SERVICE, 36);
    env.http().respond(
        "GET",
        "/api/oauth/profile",
        StubResponse::json(200, &profile("uuid-1", Some(EMAIL), None)),
    );
    stub_usage(&env, vec![("sk-new", ok_usage(10))]);

    listed(&env);

    assert_eq!(profile_probes(&env), 0);
    assert_eq!(backup(&env), Some(expired()));
}

// Not ported (one process is one usage pass, and the harness cannot inject
// faults between the pass's read and the locked re-read):
// - lock_reread_adopts_a_fresher_live_credential, lock_reread_never_adopts_a_
//   wiped_live_credential, non_oauth_live_with_moved_identity_is_never_
//   clobbered, empty_live_with_matching_identity_still_recovers,
//   identity_check_compares_organization_too, foreign_live_credential_under_
//   the_lock_is_never_consumed, adopting_a_cc_rotation_* (3): the live store
//   must differ between the pass's read and the locked re-read.
// - live_read_error_defers_instead_of_consuming, unexpected_exception_defers_
//   never_raises: need a read that fails only under the lock / raises.
// - backup_write_failure_still_persists_live_and_never_raises: the backup
//   write falls back to the `.enc` file, which shares the consume lock's dir.
// - lineage_key_binds_the_stored_email_too, verdict_does_not_survive_a_slot_
//   identity_change, expiry_after_unresynced_rotation_consumes_live_when_
//   verified, expiry_after_tolerated_backup_write_failure_consumes_live: need
//   an in-process verdict seeded by hand or two passes in one process.
