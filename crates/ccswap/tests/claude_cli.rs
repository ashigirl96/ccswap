mod support;

use serde_json::{Value, json};
use support::help::{CLAUDE_VERBS, group, lists, options};
use support::{KEYCHAIN_ACCOUNT, Output, PROG, TestEnv};

const BACKUP_SERVICE: &str = "claude-swap";
const ACCOUNTS: [(u32, &str); 3] = [
    (1, "a@example.com"),
    (2, "b@example.com"),
    (3, "c@example.com"),
];

fn record(email: &str) -> Value {
    json!({"email": email, "uuid": format!("uuid-{email}"), "organizationUuid": "",
           "organizationName": "", "added": "2024-01-01T00:00:00Z"})
}

fn seeded() -> TestEnv {
    let env = TestEnv::new();
    env.seed_sequence(json!({
        "activeAccountNumber": 1,
        "lastUpdated": "2024-01-01T00:00:00Z",
        "sequence": [1, 2, 3],
        "accounts": {
            "1": record("a@example.com"),
            "2": record("b@example.com"),
            "3": record("c@example.com"),
        }
    }));
    for (num, email) in ACCOUNTS {
        let creds = json!({"claudeAiOauth": {"accessToken": format!("sk-{num}"), "refreshToken": format!("rt-{num}")}});
        env.security().set_item(
            BACKUP_SERVICE,
            &format!("account-{num}-{email}"),
            &creds.to_string(),
        );
        env.write_file(
            format!(".claude-swap-backup/configs/.claude-config-{num}-{email}.json"),
            &json!({"oauthAccount": {"emailAddress": email, "accountUuid": format!("uuid-{email}")}})
                .to_string(),
        );
    }
    env.seed_login("a@example.com", "", "");
    env.security().set_item(
        support::ACTIVE_OAUTH_SERVICE,
        KEYCHAIN_ACCOUNT,
        &json!({"claudeAiOauth": {"accessToken": "sk-1", "refreshToken": "rt-1"}}).to_string(),
    );
    env
}

/// Each run gets its own home, so its path is the only expected difference.
fn normalized(env: &TestEnv, out: Output) -> (i32, String, String) {
    let home = env.home().display().to_string();
    (
        out.code,
        out.stdout.replace(&home, "~"),
        out.stderr.replace(&home, "~"),
    )
}

fn run_both(args: &[&str], stdin: &str) -> ((i32, String, String), (i32, String, String)) {
    let bare_env = seeded();
    let bare = bare_env.cmd(args).stdin(stdin).run();
    let namespaced_env = seeded();
    let namespaced_args: Vec<&str> = std::iter::once("claude")
        .chain(args.iter().copied())
        .collect();
    let namespaced = namespaced_env.cmd(&namespaced_args).stdin(stdin).run();
    (
        normalized(&bare_env, bare),
        normalized(&namespaced_env, namespaced),
    )
}

#[test]
fn every_claude_verb_behaves_like_the_bare_verb() {
    let cases: &[(&[&str], &str)] = &[
        (&["add"], ""),
        (&["add", "--slot", "abc"], ""),
        (&["add-token", "not-a-token", "--slot", "7"], ""),
        (&["list"], ""),
        (&["ls"], ""),
        (&["list", "--json"], ""),
        (&["status"], ""),
        (&["status", "--json"], ""),
        (&["switch"], ""),
        (&["switch", "2"], ""),
        (&["switch", "9", "--json"], ""),
        (&["remove", "3"], "y\n"),
        (&["rm", "3"], ""),
        (&["disable", "2"], ""),
        (&["enable", "2"], ""),
        (&["alias", "2", "bee"], ""),
        (&["alias"], ""),
        (&["swap", "1", "2"], ""),
        (&["move", "3", "5"], ""),
        (&["run", "9"], ""),
        (&["map"], ""),
        (&["unmap"], ""),
        (&["auto", "--help"], ""),
        (&["unclaimed"], ""),
        (&["export", "-", "--account", "9"], ""),
        (&["import", "missing.json"], ""),
    ];
    for (args, stdin) in cases {
        let (bare, namespaced) = run_both(args, stdin);
        assert_eq!(namespaced, bare, "claude {args:?}");
    }
}

#[test]
fn claude_list_json_matches_bare_list_json_and_the_original_flag() {
    let env = seeded();
    let bare = env.run(["list", "--json"]);
    bare.assert_code(0);
    assert_eq!(bare.json()["accounts"].as_array().map(Vec::len), Some(3));
    for args in [&["claude", "list", "--json"][..], &["--list", "--json"]] {
        let out = env.run(args);
        out.assert_code(0);
        assert_eq!(out.json(), bare.json(), "{args:?}");
    }
}

#[test]
fn claude_remove_with_yes_skips_the_prompt() {
    for flag in ["-y", "--yes"] {
        let env = seeded();

        let out = env.cmd(["claude", "remove", "2", flag]).stdin("n\n").run();

        out.assert_code(0);
        assert_eq!(out.stdout, "Removed Account-2 (b@example.com)\n", "{flag}");
        assert!(env.sequence()["accounts"].get("2").is_none(), "{flag}");
        assert_eq!(
            env.security()
                .item(BACKUP_SERVICE, "account-2-b@example.com"),
            None,
            "{flag}"
        );
    }
}

#[test]
fn claude_help_lists_only_the_claude_code_commands() {
    let env = TestEnv::new();
    for flag in ["--help", "-h", "help"] {
        let out = env.run(["claude", flag]);
        out.assert_code(0);
        assert!(
            out.stdout.starts_with(&format!(
                "usage: {PROG} claude <command> [args] [options]\n"
            )),
            "{flag}: {}",
            out.stdout
        );
        let commands = group(&out.stdout, "Commands:");
        for verb in CLAUDE_VERBS {
            assert!(
                lists(&commands, &format!("claude {verb}")),
                "{flag}: claude {verb}"
            );
        }
        for other in ["codex", "menubar", "tui", "purge", "config", "upgrade"] {
            assert!(
                !out.stdout.contains(&format!("{PROG} {other}")),
                "{flag}: {other}"
            );
        }
    }
}

#[test]
fn unknown_verbs_point_at_their_namespace_help() {
    let env = TestEnv::new();
    for namespace in ["claude", "codex"] {
        let out = env.run([namespace, "frobnicate"]);
        out.assert_code(2);
        assert!(
            out.stderr.contains(&format!(
                "{PROG} {namespace}: error: unknown command 'frobnicate' — try '{PROG} {namespace} --help'\n"
            )),
            "{}",
            out.stderr
        );
        assert_eq!(out.stdout, "");
    }

    let out = env.run(["claude"]);
    out.assert_code(2);
    assert!(
        out.stderr
            .contains(&format!("no command given — try '{PROG} claude --help'")),
        "{}",
        out.stderr
    );

    let out = env.run(["claude", "menubar"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains("unknown command 'menubar'"),
        "{}",
        out.stderr
    );
}

#[test]
fn each_claude_verb_help_lists_only_its_own_options() {
    let cases: &[(&str, &[&str])] = &[
        (
            "list",
            &["-h", "--help", "--token-status", "--json", "--debug"],
        ),
        ("status", &["-h", "--help", "--json", "--debug"]),
        (
            "switch",
            &[
                "-h",
                "--help",
                "--strategy",
                "--model",
                "--force",
                "--json",
                "--debug",
            ],
        ),
        ("add", &["-h", "--help", "--slot", "--alias", "--debug"]),
        (
            "add-token",
            &["-h", "--help", "--email", "--slot", "--debug"],
        ),
        ("remove", &["-h", "--help", "-y", "--yes", "--debug"]),
        ("disable", &["-h", "--help", "--debug"]),
        ("enable", &["-h", "--help", "--debug"]),
        (
            "export",
            &["-h", "--help", "--account", "--full", "--debug"],
        ),
        ("import", &["-h", "--help", "--force", "--debug"]),
    ];
    let env = TestEnv::new();
    for (verb, expected) in cases {
        for flag in ["--help", "-h"] {
            let out = env.run(["claude", verb, flag]);
            out.assert_code(0);
            assert!(
                out.stdout
                    .starts_with(&format!("usage: {PROG} claude {verb} [-h]")),
                "{verb} {flag}: {}",
                out.stdout
            );
            assert_eq!(options(&out.stdout), *expected, "{verb} {flag}");
            assert!(!out.stdout.contains("use with"), "{verb}: {}", out.stdout);
        }
    }
}

#[test]
fn bare_verb_help_matches_the_claude_verb_help() {
    let env = TestEnv::new();
    for (args, verb) in [
        (&["list", "--help"][..], "list"),
        (&["ls", "-h"], "list"),
        (&["switch", "--help"], "switch"),
        (&["switch", "2", "--help"], "switch"),
        (&["remove", "--help"], "remove"),
        (&["rm", "2", "-h"], "remove"),
        (&["add-token", "--help"], "add-token"),
        (&["export", "--help"], "export"),
        (&["alias", "--help"], "alias"),
    ] {
        let bare = env.run(args);
        let namespaced = env.run(std::iter::once("claude").chain(args.iter().copied()));
        bare.assert_code(0);
        assert_eq!(namespaced.code, 0, "{args:?}");
        assert_eq!(namespaced.stdout, bare.stdout, "{args:?}");
        assert!(
            bare.stdout
                .starts_with(&format!("usage: {PROG} claude {verb} [-h]")),
            "{args:?}: {}",
            bare.stdout
        );
    }
}

#[test]
fn every_claude_verb_help_names_the_claude_namespace() {
    let env = TestEnv::new();
    for verb in CLAUDE_VERBS {
        let bare = env.run([verb, "--help"]);
        let namespaced = env.run(["claude", verb, "--help"]);
        bare.assert_code(0);
        assert_eq!(namespaced.code, 0, "{verb}");
        assert_eq!(namespaced.stdout, bare.stdout, "{verb}");
        assert!(
            bare.stdout
                .starts_with(&format!("usage: {PROG} claude {verb} [-h]")),
            "{verb}: {}",
            bare.stdout
        );
    }
}
