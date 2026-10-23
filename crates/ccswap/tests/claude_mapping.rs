//! Ported from upstream cli.py `_map_command` / `_unmap_command` / bare
//! `_run_command`; MappingStore's own cases (tests/test_mappings.py) are unit
//! tests in ccswap-core's `mappings`.
//! Upstream loses pre-exec stdout under a pipe (see tests/claude_session.rs), so the
//! bare-run notices are asserted for ccswap only.

mod support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{ACTIVE_OAUTH_SERVICE, ClaudeCall, KEYCHAIN_ACCOUNT, PROG, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";

fn email(num: u32) -> String {
    format!("{}@example.com", ["", "a", "b", "c"][num as usize])
}

fn creds(num: u32) -> String {
    json!({"claudeAiOauth": {"accessToken": format!("sk-ant-oat01-{num}"), "expiresAt": 9_999_999_999_000_i64}})
        .to_string()
}

fn oauth_account(num: u32) -> Value {
    json!({"emailAddress": email(num), "accountUuid": format!("uuid-{num}"),
           "organizationUuid": format!("org-{num}"), "organizationName": "Acme"})
}

fn seed(env: &TestEnv) {
    let accounts: serde_json::Map<String, Value> = (1..=3)
        .map(|n| {
            let (org, org_name) = if n == 3 {
                (String::new(), "")
            } else {
                (format!("org-{n}"), "Acme")
            };
            (
                n.to_string(),
                json!({"email": email(n), "uuid": format!("uuid-{n}"), "organizationUuid": org,
                       "organizationName": org_name, "added": "2024-01-01T00:00:00Z"}),
            )
        })
        .collect();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
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
            &json!({"oauthAccount": oauth_account(n)}).to_string(),
        );
    }
    env.write_json(
        env.global_config_path(),
        &json!({"oauthAccount": oauth_account(1)}),
    );
    env.security()
        .set_item(ACTIVE_OAUTH_SERVICE, KEYCHAIN_ACCOUNT, &creds(1));
}

fn dir(env: &TestEnv, rel: &str) -> PathBuf {
    let path = env.root().join(rel);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn key(path: &Path) -> String {
    path.display().to_string()
}

fn mappings(env: &TestEnv) -> Value {
    env.read_json(env.store_dir().join("mappings.json"))
}

fn launched(env: &TestEnv) -> Option<ClaudeCall> {
    env.claude()
        .calls()
        .into_iter()
        .rfind(|c| c.argv != ["auth", "status", "--json"])
}

#[test]
fn map_records_the_account_identity_under_the_resolved_path() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "work/app");

    let out = env.run(["map", "2", repo.to_str().unwrap()]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("Mapped {} → Account-2 (b@example.com)\n", key(&repo))
    );
    let data = mappings(&env);
    assert_eq!(data["schemaVersion"], json!(1));
    let entry = &data["mappings"][key(&repo)];
    assert_eq!(entry["email"], json!("b@example.com"));
    assert_eq!(entry["organizationUuid"], json!("org-2"));
    assert!(entry["added"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(data["mappings"].as_object().unwrap().len(), 1);
}

#[test]
fn map_without_a_path_maps_the_working_directory() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");

    env.cmd(["map", "c@example.com"])
        .current_dir(&repo)
        .run()
        .assert_code(0);

    let entry = &mappings(&env)["mappings"][key(&repo)];
    assert_eq!(entry["email"], json!("c@example.com"));
    assert_eq!(entry["organizationUuid"], json!(""));
}

#[test]
fn map_keys_a_symlinked_or_relative_path_by_its_real_location() {
    let env = TestEnv::new();
    seed(&env);
    let real = dir(&env, "real/app");
    std::os::unix::fs::symlink(env.root().join("real"), env.root().join("link")).unwrap();

    env.cmd(["map", "1", "../link/app/"])
        .current_dir(dir(&env, "elsewhere"))
        .run()
        .assert_code(0);

    let data = mappings(&env);
    let keys: Vec<&String> = data["mappings"].as_object().unwrap().keys().collect();
    assert_eq!(keys, [&key(&real)]);
}

#[test]
fn remapping_a_directory_shows_the_previous_account() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");
    let path = repo.to_str().unwrap();
    env.run(["map", "1", path]).assert_code(0);

    let out = env.run(["map", "2", path]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!("Mapped {path} → Account-2 (b@example.com) (was a@example.com)\n")
    );
    let again = env.run(["map", "2", path]);
    assert_eq!(
        again.stdout,
        format!("Mapped {path} → Account-2 (b@example.com)\n")
    );
    assert_eq!(mappings(&env)["mappings"].as_object().unwrap().len(), 1);
}

#[test]
fn mapping_a_missing_directory_warns_and_maps_it_anyway() {
    let env = TestEnv::new();
    seed(&env);
    let missing = env.root().join("not-yet");

    let out = env.run(["map", "1", missing.to_str().unwrap()]);

    out.assert_code(0);
    assert!(
        out.stdout.starts_with(&format!(
            "Warning: {} is not an existing directory (mapping it anyway)\n",
            missing.display()
        )),
        "{}",
        out.stdout
    );
    assert!(mappings(&env)["mappings"].get(key(&missing)).is_some());
}

#[test]
fn mapping_an_unknown_account_fails_without_writing() {
    let env = TestEnv::new();
    seed(&env);

    let out = env.run(["map", "9", env.root().to_str().unwrap()]);

    out.assert_code(1);
    assert_eq!(out.stderr, "Error: Account-9 does not exist\n");
    assert!(!env.store_dir().join("mappings.json").exists());
}

#[test]
fn map_without_arguments_lists_mappings_sorted_with_removed_accounts() {
    let env = TestEnv::new();
    seed(&env);
    let (b, a) = (dir(&env, "b"), dir(&env, "a"));
    let gone = env.root().join("c");
    env.run(["map", "3", b.to_str().unwrap()]).assert_code(0);
    env.run(["map", "2", a.to_str().unwrap()]).assert_code(0);
    env.run(["map", "1", gone.to_str().unwrap()]).assert_code(0);
    let mut data = mappings(&env);
    data["mappings"][key(&gone)]["email"] = json!("gone@example.com");
    env.write_json(env.store_dir().join("mappings.json"), &data);

    let out = env.run(["map"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        format!(
            "Directory mappings:\n  {} → 2: b@example.com [Acme]\n  {} → 3: c@example.com [personal]\n  {} → gone@example.com (account removed)\n",
            key(&a),
            key(&b),
            key(&gone)
        )
    );
}

#[test]
fn map_without_arguments_and_no_mappings_shows_how_to_add_one() {
    let env = TestEnv::new();
    seed(&env);

    let out = env.run(["map", "--debug"]);

    out.assert_code(0);
    assert_eq!(
        out.stdout,
        "No directory mappings yet.\nMap one with: ccswap map <NUM|EMAIL> [PATH]\n"
    );
}

#[test]
fn unmap_removes_the_mapping_then_reports_none_left() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");
    let other = dir(&env, "other");
    env.run(["map", "1", repo.to_str().unwrap()]).assert_code(0);
    env.run(["map", "2", other.to_str().unwrap()])
        .assert_code(0);

    let out = env.cmd(["unmap"]).current_dir(&repo).run();

    out.assert_code(0);
    assert_eq!(out.stdout, format!("Unmapped {}\n", key(&repo)));
    let data = mappings(&env);
    let keys: Vec<&String> = data["mappings"].as_object().unwrap().keys().collect();
    assert_eq!(keys, [&key(&other)]);
    let again = env.run(["unmap", repo.to_str().unwrap()]);
    again.assert_code(0);
    assert_eq!(again.stdout, format!("No mapping for {}\n", key(&repo)));
}

#[test]
fn map_and_unmap_help_and_usage_errors_follow_argparse() {
    let env = TestEnv::new();
    let p = PROG;

    let help = env.run(["map", "-h"]);
    help.assert_code(0);
    assert!(help.stdout.starts_with(&format!(
        "usage: {p} claude map [-h] [--debug] [NUM|EMAIL] [PATH]\n\nMap a stored account"
    )));
    assert!(help.stdout.contains(&format!(
        "  {p} map                           # list all mappings\n"
    )));
    let help = env.run(["unmap", "--h"]);
    help.assert_code(0);
    assert!(help.stdout.starts_with(&format!(
        "usage: {p} claude unmap [-h] [--debug] [PATH]\n\nRemove a directory → account mapping (default: current directory).\n"
    )));

    let out = env.run(["map", "a", "b", "c"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!(
            "usage: {p} map [-h] [--debug] [NUM|EMAIL] [PATH]\n{p} map: error: unrecognized arguments: c\n"
        )
    );
    let out = env.run(["unmap", "--debug=1"]);
    out.assert_code(2);
    assert_eq!(
        out.stderr,
        format!(
            "usage: {p} unmap [-h] [--debug] [PATH]\n{p} unmap: error: argument --debug: ignored explicit argument '1'\n"
        )
    );
}

#[test]
fn bare_run_launches_the_account_mapped_to_an_ancestor_in_session_mode() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");
    let deep = dir(&env, "repo/src/deep");
    env.run(["map", "2", repo.to_str().unwrap()]).assert_code(0);
    env.claude().respond(
        "auth status --json",
        &json!({"loggedIn": true, "authMethod": "claude.ai", "email": email(2), "orgId": "org-2"})
            .to_string(),
        0,
    );

    let out = env.cmd(["run", "--", "--resume"]).current_dir(&deep).run();

    out.assert_code(0);
    let call = launched(&env).expect("claude was exec'd");
    assert_eq!(call.argv, ["--resume"]);
    assert_eq!(call.cwd, key(&deep));
    assert_eq!(
        call.env["CLAUDE_CONFIG_DIR"],
        env.store_dir()
            .join("sessions/2-b_example.com")
            .display()
            .to_string()
    );
}

#[test]
fn bare_run_without_a_mapping_launches_the_default_account() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");

    let out = env
        .cmd(["run", "--", "-p", "hi"])
        .current_dir(&repo)
        .env("ANTHROPIC_API_KEY", "kept")
        .run();

    out.assert_code(0);
    let call = launched(&env).expect("claude was exec'd");
    assert_eq!(call.argv, ["-p", "hi"]);
    assert!(!call.env.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(call.env["ANTHROPIC_API_KEY"], "kept");
    assert!(!env.store_dir().join("sessions").exists());
    assert_eq!(
        out.stdout,
        format!(
            "No account mapped for {} — launching the default account.\n",
            key(&repo)
        )
    );
}

#[test]
fn bare_run_whose_mapped_account_was_removed_launches_the_default_account() {
    let env = TestEnv::new();
    seed(&env);
    let repo = dir(&env, "repo");
    env.run(["map", "2", repo.to_str().unwrap()]).assert_code(0);
    let mut data = mappings(&env);
    data["mappings"][key(&repo)]["organizationUuid"] = json!("org-other");
    env.write_json(env.store_dir().join("mappings.json"), &data);

    let out = env.cmd(["run"]).current_dir(&repo).run();

    out.assert_code(0);
    let call = launched(&env).expect("claude was exec'd");
    assert!(call.argv.is_empty());
    assert!(!call.env.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(
        out.stdout,
        "Mapped account b@example.com no longer exists — launching the default account.\n"
    );
}

#[test]
fn removing_an_account_prunes_its_mappings() {
    let env = TestEnv::new();
    seed(&env);
    let (a, b) = (dir(&env, "a"), dir(&env, "b"));
    env.run(["map", "2", a.to_str().unwrap()]).assert_code(0);
    env.run(["map", "3", b.to_str().unwrap()]).assert_code(0);

    let out = env.cmd(["remove", "2"]).stdin("y\n").run();

    out.assert_code(0);
    assert!(
        out.stdout
            .contains("Removed 1 directory mapping(s) for this account"),
        "{}",
        out.stdout
    );
    let data = mappings(&env);
    let keys: Vec<&String> = data["mappings"].as_object().unwrap().keys().collect();
    assert_eq!(keys, [&key(&b)]);
}
