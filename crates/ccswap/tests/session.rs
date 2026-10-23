//! Ported from upstream tests/test_session.py::TestRun, TestBootstrap
//! (happy_path, profile_permissions, missing_credentials, missing_config,
//! validation_failure_cleans_up, stale_marker_*), TestSharingPosix,
//! TestShareHistoryPosix (links, creates_missing_source, merges, toggle),
//! TestMcpMirror (bootstrap_launch_mirrors, no_share_*), TestGuards and
//! TestHelpers::test_session_dir_naming. Slots hold setup-token-shaped
//! credentials so neither implementation refreshes before seeding.
//!
//! Upstream's `os.execvpe` drops Python's unflushed stdout under a pipe, so
//! pre-exec lines are asserted for ccswap only.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, ClaudeCall, KEYCHAIN_ACCOUNT, Output, StubResponse, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn email(num: u32) -> String {
    ["", "a@example.com", "b@example.com", "c@example.com"][num as usize].to_string()
}

fn org(num: u32) -> String {
    format!("org-{num}")
}

fn creds(num: u32) -> String {
    json!({"claudeAiOauth": {"accessToken": format!("sk-ant-oat01-{num}"), "expiresAt": 9_999_999_999_000_i64}})
        .to_string()
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}"),
           "organizationUuid": org(num), "organizationName": "Acme"})
}

fn seed(env: &TestEnv, active: u32) {
    let accounts: serde_json::Map<String, Value> = (1..=3)
        .map(|n| {
            (
                n.to_string(),
                json!({"email": email(n), "uuid": format!("uuid-{n}"), "organizationUuid": org(n),
                       "organizationName": "Acme", "added": "2024-01-01T00:00:00Z"}),
            )
        })
        .collect();
    env.seed_sequence(json!({
        "activeAccountNumber": active,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2, 3],
        "accounts": accounts,
    }));
    for n in 1..=3 {
        env.security().set_item(
            BACKUP_SERVICE,
            &format!("account-{n}-{}", email(n)),
            &creds(n),
        );
        env.write_file(
            env.store_dir()
                .join(format!("configs/.claude-config-{n}-{}.json", email(n))),
            &json!({"oauthAccount": oauth_account(n), "theme": "light"}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(active)}),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds(active));
}

fn auth_status_as(env: &TestEnv, num: u32) {
    env.claude().respond(
        "auth status --json",
        &json!({"loggedIn": true, "authMethod": "claude.ai", "email": email(num), "orgId": org(num)})
            .to_string(),
        0,
    );
}

/// A probe that runs but does not answer: the verdict falls back to the
/// profile's files, which is how these tests force a (re)bootstrap.
fn auth_status_unanswered(env: &TestEnv) {
    env.claude().respond("auth status --json", "not json", 0);
}

/// Slot 1's live credential is an access-token rotation of its backup, so
/// switching away backs it up.
fn rotate_live_of_slot_1(env: &TestEnv) {
    let family = |access: &str| {
        json!({"claudeAiOauth": {"accessToken": access, "refreshToken": "rt-1", "expiresAt": 9_999_999_999_000_i64}})
            .to_string()
    };
    env.security()
        .set_item(BACKUP_SERVICE, "account-1-a@example.com", &family("at-1"));
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &family("at-1b"));
}

fn session_dir(env: &TestEnv, num: u32) -> PathBuf {
    env.store_dir()
        .join("sessions")
        .join(format!("{num}-{}", email(num).replace('@', "_")))
}

fn make_live(dir: &Path) {
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let pid = std::process::id();
    std::fs::write(
        dir.join("sessions").join(format!("{pid}.json")),
        json!({"pid": pid, "sessionId": "s", "cwd": "/w"}).to_string(),
    )
    .unwrap();
}

/// The exec'd claude: the last recorded call that is not the probe.
fn launched(env: &TestEnv) -> Option<ClaudeCall> {
    env.claude()
        .calls()
        .into_iter()
        .rfind(|c| c.argv != ["auth", "status", "--json"])
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn stale_marker(env: &TestEnv, num: u32) -> PathBuf {
    let dir = session_dir(env, num);
    dir.with_file_name(format!(
        ".{}.cswap-stale-credentials",
        dir.file_name().unwrap().to_string_lossy()
    ))
}

fn assert_error(out: &Output, needle: &str) {
    assert_eq!(
        out.code, 1,
        "stdout: {}\nstderr: {}",
        out.stdout, out.stderr
    );
    assert!(out.stderr.contains(needle), "stderr: {}", out.stderr);
}

#[test]
fn run_execs_claude_in_the_profile_with_forwarded_args() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);

    let out = env.run(["run", "2", "--", "--resume", "--model", "x"]);

    out.assert_code(0);
    let call = launched(&env).expect("claude was exec'd");
    assert_eq!(call.argv, ["--resume", "--model", "x"]);
    let dir = session_dir(&env, 2);
    assert_eq!(call.env["CLAUDE_CONFIG_DIR"], dir.display().to_string());
    assert!(
        out.stdout
            .contains("Launching Account-2 (b@example.com) [session mode]"),
        "{}",
        out.stdout
    );
}

#[test]
fn bootstrap_seeds_credentials_and_identity_privately() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);

    env.run(["run", "2"]).assert_code(0);

    let dir = session_dir(&env, 2);
    assert_eq!(
        dir.file_name().unwrap().to_string_lossy(),
        "2-b_example.com"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
        creds(2)
    );
    let config = env.read_json(dir.join(".claude.json"));
    assert_eq!(config["oauthAccount"], oauth_account(2));
    assert_eq!(config["hasCompletedOnboarding"], json!(true));
    assert_eq!(config["theme"], json!("light"));
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(".credentials.json")), 0o600);
    assert_eq!(mode(&dir.join(".claude.json")), 0o600);
    let probe = env
        .claude()
        .calls()
        .into_iter()
        .find(|c| c.argv == ["auth", "status", "--json"])
        .expect("probe ran");
    assert_eq!(probe.env["CLAUDE_CONFIG_DIR"], dir.display().to_string());
}

#[test]
fn rebootstrap_keeps_the_profiles_own_config_keys() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"projects": {"/p": {}}, "theme": "dark"}).to_string(),
    )
    .unwrap();
    auth_status_unanswered(&env);

    env.run(["run", "2"]).assert_code(0);

    let config = env.read_json(dir.join(".claude.json"));
    assert_eq!(config["projects"], json!({"/p": {}}));
    assert_eq!(config["theme"], json!("dark"));
    assert_eq!(config["oauthAccount"], oauth_account(2));
}

#[test]
fn auth_override_vars_are_scrubbed_from_the_session() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);

    let out = env
        .cmd(["run", "2"])
        .env("ANTHROPIC_API_KEY", "sk-ant-key")
        .env("ANTHROPIC_AUTH_TOKEN", "tok")
        .env("UNRELATED_VAR", "kept")
        .run();

    out.assert_code(0);
    let call = launched(&env).unwrap();
    assert!(!call.env.contains_key("ANTHROPIC_API_KEY"));
    assert!(!call.env.contains_key("ANTHROPIC_AUTH_TOKEN"));
    assert_eq!(call.env["UNRELATED_VAR"], "kept");
    for c in env.claude().calls() {
        assert!(!c.env.contains_key("ANTHROPIC_API_KEY"));
    }
    assert!(
        out.stdout
            .contains("Ignoring ANTHROPIC_API_KEY, ANTHROPIC_AUTH_TOKEN for this session"),
        "{}",
        out.stdout
    );
}

#[test]
fn active_default_login_launches_plain_claude_with_the_env_untouched() {
    let env = TestEnv::new();
    seed(&env, 2);

    let out = env
        .cmd(["run", "2", "--", "--resume"])
        .env("ANTHROPIC_API_KEY", "sk-ant-key")
        .run();

    out.assert_code(0);
    let call = launched(&env).unwrap();
    assert_eq!(call.argv, ["--resume"]);
    assert!(!call.env.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(call.env["ANTHROPIC_API_KEY"], "sk-ant-key");
    assert!(!session_dir(&env, 2).exists());
    assert!(out.stdout.contains("already the active default login"));
}

#[test]
fn require_session_refuses_the_active_default_login() {
    let env = TestEnv::new();
    seed(&env, 2);

    let out = env.run(["run", "2", "--require-session"]);

    assert_error(&out, "is the active default login");
    assert!(env.claude().calls().is_empty());
}

#[test]
fn require_session_is_inert_for_another_account() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);

    env.run(["run", "2", "--require-session"]).assert_code(0);

    let call = launched(&env).unwrap();
    assert_eq!(
        call.env["CLAUDE_CONFIG_DIR"],
        session_dir(&env, 2).display().to_string()
    );
}

#[test]
fn preset_config_dir_disables_the_fast_path() {
    let env = TestEnv::new();
    seed(&env, 2);
    auth_status_as(&env, 2);
    let elsewhere = env.root().join("elsewhere");

    let out = env
        .cmd(["run", "2"])
        .env("CLAUDE_CONFIG_DIR", &elsewhere.display().to_string())
        .run();

    out.assert_code(0);
    let call = launched(&env).unwrap();
    assert_eq!(
        call.env["CLAUDE_CONFIG_DIR"],
        session_dir(&env, 2).display().to_string()
    );
    assert!(out.stdout.contains("overriding it for this launch"));
}

#[test]
fn claude_missing_from_path_is_an_error() {
    let env = TestEnv::new();
    seed(&env, 1);

    let path = format!(
        "{}:{}:/usr/bin:/bin",
        env.launchctl().bin_dir().display(),
        env.codex().bin_dir().display()
    );
    let out = env.cmd(["run", "2"]).env("PATH", &path).run();

    assert_error(&out, "'claude' was not found on PATH");
}

#[test]
fn api_key_slots_are_refused() {
    let env = TestEnv::new();
    seed(&env, 1);
    let mut data = env.sequence();
    data["accounts"]["2"]["kind"] = json!("api_key");
    env.seed_sequence(data);

    let out = env.run(["run", "2"]);

    assert_error(&out, "is an API-key account");
    assert!(env.claude().calls().is_empty());
}

#[test]
fn unknown_account_is_an_error() {
    let env = TestEnv::new();
    seed(&env, 1);

    assert_error(&env.run(["run", "9"]), "Account-9 does not exist");
    assert_error(
        &env.run(["run", "zed@example.com"]),
        "No account found with identifier: zed@example.com",
    );
}

#[test]
fn auth_status_for_another_account_refuses_and_cleans_up() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 3);

    let out = env.run(["run", "2"]);

    assert_error(&out, "failed validation");
    assert!(launched(&env).is_none());
    assert!(!session_dir(&env, 2).exists());
}

#[test]
fn missing_credentials_refuse_without_launching() {
    let env = TestEnv::new();
    seed(&env, 1);
    env.security().fail_command_for_item(
        "find-generic-password",
        BACKUP_SERVICE,
        "account-2-b@example.com",
        44,
    );
    auth_status_as(&env, 2);

    let out = env.run(["run", "2"]);

    assert_error(&out, "Account-2 has no stored credentials");
    assert!(launched(&env).is_none());
}

#[test]
fn missing_config_backup_refuses() {
    let env = TestEnv::new();
    seed(&env, 1);
    std::fs::remove_file(
        env.store_dir()
            .join("configs/.claude-config-2-b@example.com.json"),
    )
    .unwrap();
    auth_status_as(&env, 2);

    assert_error(
        &env.run(["run", "2"]),
        "Account-2 has no stored config backup",
    );
}

#[test]
fn valid_profile_is_reused_without_reseeding() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    env.run(["run", "2"]).assert_code(0);
    let creds_path = session_dir(&env, 2).join(".credentials.json");
    std::fs::write(&creds_path, creds(2)).unwrap();
    let before = std::fs::metadata(&creds_path).unwrap().modified().unwrap();

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(
        std::fs::metadata(&creds_path).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn stale_marker_forces_rebootstrap_once_the_session_exits() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    env.run(["run", "2"]).assert_code(0);
    let dir = session_dir(&env, 2);
    std::fs::write(dir.join(".credentials.json"), "superseded").unwrap();
    std::fs::write(stale_marker(&env, 2), "").unwrap();
    auth_status_unanswered(&env);

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
        creds(2)
    );
    assert!(!stale_marker(&env, 2).exists());
}

#[test]
fn stale_marker_is_kept_while_the_session_is_live() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    env.run(["run", "2"]).assert_code(0);
    let dir = session_dir(&env, 2);
    std::fs::write(dir.join(".credentials.json"), "running-copy").unwrap();
    std::fs::write(stale_marker(&env, 2), "").unwrap();
    make_live(&dir);

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(dir.join(".credentials.json")).unwrap(),
        "running-copy"
    );
    assert!(stale_marker(&env, 2).exists());
}

fn seed_default_profile(env: &TestEnv) -> PathBuf {
    let home = env.home().join(".claude");
    std::fs::write(home.join("settings.json"), "{}").unwrap();
    std::fs::create_dir_all(home.join("skills")).unwrap();
    home
}

#[test]
fn shared_items_are_symlinked_and_listed_in_the_manifest() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let home = seed_default_profile(&env);

    env.run(["run", "2"]).assert_code(0);

    let dir = session_dir(&env, 2);
    for name in ["settings.json", "skills"] {
        assert_eq!(std::fs::read_link(dir.join(name)).unwrap(), home.join(name));
    }
    assert!(!dir.join("CLAUDE.md").exists());
    assert!(!dir.join("projects").exists());
    assert_eq!(
        env.read_json(dir.join(".cswap-shared.json")),
        json!({"items": ["settings.json", "skills"], "mode": "symlink"})
    );
}

#[test]
fn shared_link_points_at_the_resolved_target() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let real = env.root().join("dotfiles-settings.json");
    std::fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, env.home().join(".claude/settings.json")).unwrap();

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(
        std::fs::read_link(session_dir(&env, 2).join("settings.json")).unwrap(),
        real
    );
}

#[test]
fn existing_profile_copy_is_never_replaced() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    seed_default_profile(&env);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), "mine").unwrap();

    let out = env.run(["run", "2"]);

    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(dir.join("settings.json")).unwrap(),
        "mine"
    );
    assert!(
        out.stdout
            .contains("Not sharing settings.json: the session profile already has its own copy.")
    );
}

#[test]
fn no_share_removes_only_managed_links() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let home = seed_default_profile(&env);
    env.run(["run", "2"]).assert_code(0);
    let dir = session_dir(&env, 2);
    std::fs::write(dir.join("CLAUDE.md"), "profile's own").unwrap();

    env.run(["run", "2", "--no-share"]).assert_code(0);

    assert!(!dir.join("settings.json").exists());
    assert!(!dir.join("skills").exists());
    assert!(home.join("skills").is_dir());
    assert_eq!(
        std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap(),
        "profile's own"
    );
    assert!(!dir.join(".cswap-shared.json").exists());
}

#[test]
fn share_history_links_history_and_creates_missing_sources() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);

    env.run(["run", "2", "--share-history"]).assert_code(0);

    let home = env.home().join(".claude");
    let dir = session_dir(&env, 2);
    assert!(home.join("projects").is_dir());
    assert_eq!(mode(&home.join("projects")), 0o700);
    assert_eq!(mode(&home.join("history.jsonl")), 0o600);
    for name in ["projects", "history.jsonl"] {
        assert_eq!(std::fs::read_link(dir.join(name)).unwrap(), home.join(name));
    }
}

#[test]
fn share_history_merges_the_profiles_own_history_first() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let home = env.home().join(".claude");
    std::fs::create_dir_all(home.join("projects/p")).unwrap();
    std::fs::write(home.join("projects/p/a.jsonl"), "home").unwrap();
    std::fs::write(home.join("history.jsonl"), "one\n").unwrap();
    env.run(["run", "2"]).assert_code(0);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(dir.join("projects/p")).unwrap();
    std::fs::write(dir.join("projects/p/a.jsonl"), "profile").unwrap();
    std::fs::write(dir.join("projects/p/b.jsonl"), "only-profile").unwrap();
    std::fs::write(dir.join("history.jsonl"), "one\ntwo\n").unwrap();

    env.run(["run", "2", "--share-history"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(home.join("projects/p/a.jsonl")).unwrap(),
        "home"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("projects/p/b.jsonl")).unwrap(),
        "only-profile"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("history.jsonl")).unwrap(),
        "one\ntwo\n"
    );
    assert!(dir.join("projects").is_symlink());

    env.run(["run", "2", "--no-share-history"]).assert_code(0);

    assert!(!dir.join("projects").exists());
    assert!(home.join("projects/p/b.jsonl").exists());
}

#[test]
fn share_history_waits_while_the_profile_is_live() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    env.run(["run", "2"]).assert_code(0);
    let dir = session_dir(&env, 2);
    std::fs::write(dir.join("history.jsonl"), "mine\n").unwrap();
    make_live(&dir);

    env.run(["run", "2", "--share-history"]).assert_code(0);

    assert!(!dir.join("history.jsonl").is_symlink());
    assert_eq!(
        std::fs::read_to_string(dir.join("history.jsonl")).unwrap(),
        "mine\n"
    );
}

#[test]
fn mcp_servers_are_mirrored_and_removed_with_no_share() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let servers = json!({"fs": {"command": "npx", "args": ["fs"]}});
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(1), "mcpServers": servers}),
    );

    env.run(["run", "2"]).assert_code(0);

    let dir = session_dir(&env, 2);
    assert_eq!(
        env.read_json(dir.join(".claude.json"))["mcpServers"],
        servers
    );
    assert!(dir.join(".cswap-mcp-mirror-v1").exists());

    env.run(["run", "2", "--no-share"]).assert_code(0);

    assert!(
        env.read_json(dir.join(".claude.json"))
            .get("mcpServers")
            .is_none()
    );
}

#[test]
fn mcp_first_mirror_stashes_displaced_definitions() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"mcpServers": {"local": {"command": "x"}, "fs": {"command": "npx"}}}).to_string(),
    )
    .unwrap();
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(1), "mcpServers": {"fs": {"command": "npx"}}}),
    );

    env.run(["run", "2"]).assert_code(0);

    assert_eq!(
        env.read_json(dir.join(".cswap-mcp-displaced.json")),
        json!({"schemaVersion": 1, "mcpServers": {"local": {"command": "x"}}})
    );
    assert_eq!(
        env.read_json(dir.join(".claude.json"))["mcpServers"],
        json!({"fs": {"command": "npx"}})
    );
}

#[test]
fn mcp_no_share_before_adoption_leaves_local_servers() {
    let env = TestEnv::new();
    seed(&env, 1);
    auth_status_as(&env, 2);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"mcpServers": {"local": {"command": "x"}}}).to_string(),
    )
    .unwrap();

    env.run(["run", "2", "--no-share"]).assert_code(0);

    assert_eq!(
        env.read_json(dir.join(".claude.json"))["mcpServers"],
        json!({"local": {"command": "x"}})
    );
}

#[test]
fn remove_is_refused_while_the_slot_has_a_live_session() {
    let env = TestEnv::new();
    seed(&env, 1);
    make_live(&session_dir(&env, 2));

    let out = env.cmd(["remove", "2"]).stdin("y\n").run();

    assert_error(&out, "has a live session-mode Claude instance");
    assert!(!out.stdout.contains("Are you sure"));
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com"),
        Some(creds(2))
    );
}

#[test]
fn remove_deletes_the_session_profile_and_its_keychain_item() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();
    let service = profile_service(&dir);
    env.security().set_item(&service, KEYCHAIN_ACCOUNT, "creds");
    std::fs::write(stale_marker(&env, 2), "").unwrap();

    env.cmd(["remove", "2"]).stdin("y\n").run().assert_code(0);

    assert!(!dir.exists());
    assert!(!stale_marker(&env, 2).exists());
    assert_eq!(env.security().item(&service, KEYCHAIN_ACCOUNT), None);
    let log = std::fs::read_to_string(env.store_dir().join("claude-swap.log")).unwrap();
    assert!(
        log.contains(&format!(
            "INFO - Removed session profile for account 2 at {}",
            dir.display()
        )),
        "{log}"
    );
}

#[test]
fn purge_is_refused_while_any_session_is_live() {
    let env = TestEnv::new();
    seed(&env, 1);
    make_live(&session_dir(&env, 2));

    let out = env.cmd(["purge"]).stdin("y\n").run();

    assert_error(&out, "Exit them first, then retry --purge.");
    assert!(env.store_dir().exists());
}

#[test]
fn purge_is_refused_over_unreadable_session_records() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::write(dir.join("sessions/1.json"), "not json").unwrap();

    let out = env.cmd(["purge"]).stdin("y\n").run();

    assert_error(
        &out,
        "Session records that could not be read: 2-b_example.com (1 record(s))",
    );
}

#[test]
fn overwriting_a_live_slot_is_refused() {
    let env = TestEnv::new();
    seed(&env, 1);
    make_live(&session_dir(&env, 2));

    let out = env
        .cmd([
            "add-token",
            "tok",
            "--email",
            "new@example.com",
            "--slot",
            "2",
        ])
        .stdin("y\n")
        .run();

    assert_error(&out, "Exit it first, then retry the operation.");
    assert_eq!(
        env.sequence()["accounts"]["2"]["email"],
        json!("b@example.com")
    );
    assert_eq!(
        env.security()
            .item(BACKUP_SERVICE, "account-2-b@example.com"),
        Some(creds(2))
    );
}

#[test]
fn swap_and_move_are_refused_while_a_slot_is_live() {
    let env = TestEnv::new();
    seed(&env, 1);
    make_live(&session_dir(&env, 2));

    assert_error(&env.run(["swap", "1", "2"]), "retry --swap-accounts.");
    assert_error(&env.run(["move", "2", "7"]), "retry --move-account.");
    assert_eq!(env.sequence()["sequence"], json!([1, 2, 3]));
}

#[test]
fn switch_inside_a_session_profile_is_refused() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = session_dir(&env, 2);
    std::fs::create_dir_all(&dir).unwrap();

    let out = env
        .cmd(["switch", "3"])
        .env("CLAUDE_CONFIG_DIR", &dir.display().to_string())
        .run();

    assert_error(&out, "inside a cswap run session profile");
}

#[test]
fn switch_to_a_live_session_account_warns_marks_the_outgoing_profile_and_completes() {
    let env = TestEnv::new();
    seed(&env, 1);
    rotate_live_of_slot_1(&env);
    make_live(&session_dir(&env, 2));
    let outgoing = session_dir(&env, 1);
    std::fs::create_dir_all(&outgoing).unwrap();
    std::fs::write(outgoing.join(".credentials.json"), creds(1)).unwrap();

    let out = env.run(["switch", "2"]);

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("Account-2 (b@example.com) has a live session-mode Claude instance"),
        "{}",
        out.stdout
    );
    assert_eq!(env.sequence()["activeAccountNumber"], json!(2));
    assert!(!outgoing.join(".credentials.json").exists());
}

#[test]
fn backup_write_under_a_live_profile_only_marks_it_stale() {
    let env = TestEnv::new();
    seed(&env, 1);
    rotate_live_of_slot_1(&env);
    let outgoing = session_dir(&env, 1);
    std::fs::create_dir_all(&outgoing).unwrap();
    std::fs::write(outgoing.join(".credentials.json"), "running-copy").unwrap();
    make_live(&outgoing);

    env.run(["switch", "2"]).assert_code(0);

    assert_eq!(
        std::fs::read_to_string(outgoing.join(".credentials.json")).unwrap(),
        "running-copy"
    );
    assert!(stale_marker(&env, 1).exists());
}

#[test]
fn import_force_warns_about_a_live_session_and_marks_it_stale() {
    let env = TestEnv::new();
    seed(&env, 1);
    let export = env.root().join("export.json");
    env.run(["export", export.to_str().unwrap()]).assert_code(0);
    let dir = session_dir(&env, 2);
    make_live(&dir);

    let out = env.run(["import", export.to_str().unwrap(), "--force"]);

    out.assert_code(0);
    assert!(
        out.stderr
            .contains("Warning: b@example.com (slot 2) has a live session-mode instance (PID"),
        "{}",
        out.stderr
    );
    assert!(stale_marker(&env, 2).exists());
}

fn usage_bearers(env: &TestEnv) -> Vec<String> {
    env.http()
        .requests()
        .iter()
        .filter(|r| r.path() == "/api/oauth/usage")
        .filter_map(|r| r.header("Authorization").map(str::to_string))
        .collect()
}

fn profile_with_token(env: &TestEnv, num: u32, access: &str, expires_at: i64) -> PathBuf {
    let dir = session_dir(env, num);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        json!({"claudeAiOauth": {"accessToken": access, "refreshToken": "rt-s", "expiresAt": expires_at}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"oauthAccount": oauth_account(num)}).to_string(),
    )
    .unwrap();
    dir
}

#[test]
fn list_fetches_a_session_owned_slot_with_the_profiles_token() {
    let env = TestEnv::new();
    seed(&env, 1);
    profile_with_token(&env, 2, "at-session", 9_999_999_999_000);
    env.http().respond(
        "GET",
        "/api/oauth/usage",
        StubResponse::json(200, r#"{"five_hour": {"utilization": 10.0}}"#),
    );

    env.run(["list", "--json"]).assert_code(0);

    let bearers = usage_bearers(&env);
    assert!(
        bearers.contains(&"Bearer at-session".to_string()),
        "{bearers:?}"
    );
    assert!(
        !bearers.contains(&"Bearer sk-ant-oat01-2".to_string()),
        "{bearers:?}"
    );
}

#[test]
fn list_ignores_a_profile_logged_in_as_another_account() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = profile_with_token(&env, 2, "at-session", 9_999_999_999_000);
    std::fs::write(
        dir.join(".claude.json"),
        json!({"oauthAccount": oauth_account(3)}).to_string(),
    )
    .unwrap();
    env.http().respond(
        "GET",
        "/api/oauth/usage",
        StubResponse::json(200, r#"{"five_hour": {"utilization": 10.0}}"#),
    );

    env.run(["list", "--json"]).assert_code(0);

    let bearers = usage_bearers(&env);
    assert!(
        !bearers.contains(&"Bearer at-session".to_string()),
        "{bearers:?}"
    );
    assert!(
        bearers.contains(&"Bearer sk-ant-oat01-2".to_string()),
        "{bearers:?}"
    );
}

#[test]
fn list_does_not_request_an_expired_live_session_token() {
    let env = TestEnv::new();
    seed(&env, 1);
    let dir = profile_with_token(&env, 2, "at-session", 1_000);
    make_live(&dir);
    env.http().respond(
        "GET",
        "/api/oauth/usage",
        StubResponse::json(200, r#"{"five_hour": {"utilization": 10.0}}"#),
    );

    let out = env.run(["list", "--json"]);

    out.assert_code(0);
    let bearers = usage_bearers(&env);
    assert!(
        !bearers.contains(&"Bearer at-session".to_string()),
        "{bearers:?}"
    );
    assert!(
        !bearers.contains(&"Bearer sk-ant-oat01-2".to_string()),
        "{bearers:?}"
    );
}

#[test]
fn run_parser_errors_match_argparse() {
    let env = TestEnv::new();
    let prog = support::PROG;
    let usage = format!(
        "usage: {prog} run [-h] [--no-share] [--share-history | --no-share-history]\n{pad}[--require-session] [--debug]\n{pad}[NUM|EMAIL]\n",
        pad = " ".repeat(format!("usage: {prog} run ").len())
    );
    for (args, msg) in [
        (
            vec!["run", "--bogus", "1"],
            "unrecognized arguments: --bogus",
        ),
        (
            vec!["run", "1", "2", "--", "x"],
            "unrecognized arguments: 2",
        ),
        (
            vec!["run", "--share-history=x", "1"],
            "argument --share-history/--no-share-history: ignored explicit argument 'x'",
        ),
        (
            vec!["run", "--no-sh", "1"],
            "ambiguous option: --no-sh could match --no-share, --no-share-history",
        ),
    ] {
        let out = env.run(&args);
        out.assert_code(2);
        assert_eq!(
            out.stderr,
            format!("{usage}{prog} run: error: {msg}\n"),
            "{args:?}"
        );
    }
}

#[test]
fn run_help_lists_the_flags() {
    let env = TestEnv::new();
    let out = env.run(["run", "--help"]);
    out.assert_code(0);
    for flag in [
        "--no-share",
        "--share-history, --no-share-history",
        "--require-session",
    ] {
        assert!(out.stdout.contains(flag), "{}", out.stdout);
    }
}

/// Claude Code's hashed Keychain service for a config dir (sha256, 8 hex).
fn profile_service(dir: &Path) -> String {
    use std::io::Write;
    let mut child = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(dir.display().to_string().as_bytes())
        .unwrap();
    let digest = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    format!("Claude Code-credentials-{}", &digest[..8])
}
