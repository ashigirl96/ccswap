mod support;

use serde_json::{Value, json};
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

const CLAUDE_VERBS: [&str; 18] = [
    "add",
    "add-token",
    "list",
    "status",
    "switch",
    "remove",
    "disable",
    "enable",
    "alias",
    "swap",
    "move",
    "run",
    "map",
    "unmap",
    "auto",
    "unclaimed",
    "export",
    "import",
];

const APP_COMMANDS: [&str; 8] = [
    "help", "tui", "watch", "menubar", "widget", "config", "upgrade", "purge",
];

/// The command lines between `heading` and the next blank line.
fn group<'a>(help: &'a str, heading: &str) -> Vec<&'a str> {
    help.lines()
        .skip_while(|line| !line.starts_with(heading))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .collect()
}

fn lists(lines: &[&str], command: &str) -> bool {
    lines.iter().any(|line| {
        line.starts_with(&format!("  {PROG} {command} ")) || *line == format!("  {PROG} {command}")
    })
}

#[test]
fn main_help_groups_commands_by_claude_code_codex_and_app() {
    let env = TestEnv::new();
    let out = env.run(["help"]);
    out.assert_code(0);
    let help = out.stdout.as_str();
    let headings = ["Claude Code accounts", "Codex CLI accounts", "App commands"];
    let positions: Vec<usize> = headings
        .iter()
        .map(|h| {
            help.find(&format!("\n{h}"))
                .unwrap_or_else(|| panic!("{h:?} in\n{help}"))
        })
        .collect();
    assert!(
        positions.is_sorted(),
        "{headings:?} out of order in\n{help}"
    );

    let claude = group(help, headings[0]);
    for verb in CLAUDE_VERBS {
        assert!(
            lists(&claude, &format!("claude {verb}")),
            "claude {verb} in {claude:#?}"
        );
        assert!(!lists(&claude, verb), "bare {verb} in {claude:#?}");
    }
    assert!(lists(&claude, "claude --help"), "{claude:#?}");
    assert!(
        claude
            .iter()
            .any(|line| line.contains("without the 'claude' prefix")),
        "{claude:#?}"
    );

    let codex = group(help, headings[1]);
    for verb in ["add", "login", "list", "switch", "remove", "alias"] {
        assert!(
            lists(&codex, &format!("codex {verb}")),
            "codex {verb} in {codex:#?}"
        );
    }

    let app = group(help, headings[2]);
    for command in APP_COMMANDS {
        assert!(lists(&app, command), "{command} in {app:#?}");
    }
    assert!(
        !app.iter()
            .any(|line| line.contains(" claude ") || line.contains(" codex "))
    );
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
