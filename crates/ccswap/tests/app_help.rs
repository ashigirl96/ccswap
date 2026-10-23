mod support;

use support::help::{CLAUDE_VERBS, CODEX_VERBS, group, lists, options};
use support::{PROG, TestEnv};

const APP_COMMANDS: [&str; 8] = [
    "help", "tui", "watch", "menubar", "widget", "config", "upgrade", "purge",
];

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
    for verb in CODEX_VERBS {
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
fn overall_helps_list_only_the_options_every_command_takes() {
    let env = TestEnv::new();
    let cases: &[(&[&str], &[&str])] = &[
        (&["help"], &["-h", "--help", "--version", "--debug"]),
        (&["claude", "--help"], &["-h", "--help", "--debug"]),
        (&["codex", "--help"], &["-h", "--help", "--debug"]),
    ];
    for (args, expected) in cases {
        let out = env.run(*args);
        out.assert_code(0);
        assert_eq!(options(&out.stdout), *expected, "{args:?}");
    }
}

#[test]
fn no_help_says_which_command_an_option_is_for() {
    let env = TestEnv::new();
    let mut helps: Vec<Vec<&str>> = vec![
        vec!["help"],
        vec!["claude", "--help"],
        vec!["codex", "--help"],
    ];
    helps.extend(
        CLAUDE_VERBS
            .iter()
            .map(|verb| vec!["claude", verb, "--help"]),
    );
    helps.extend(CODEX_VERBS.iter().map(|verb| vec!["codex", verb, "--help"]));
    helps.extend(
        ["tui", "watch", "menubar", "widget", "upgrade", "purge"]
            .iter()
            .map(|command| vec![command, "--help"]),
    );
    for args in helps {
        let out = env.run(&args);
        out.assert_code(0);
        for note in ["use with", "With '", "With bare '"] {
            assert!(
                !out.stdout.contains(note),
                "{args:?} says {note:?}:\n{}",
                out.stdout
            );
        }
    }
}

#[test]
fn app_command_help_lists_only_its_own_options() {
    let env = TestEnv::new();
    let cases: &[(&str, &[&str])] = &[
        (
            "menubar",
            &[
                "-h",
                "--help",
                "--install-service",
                "--uninstall-service",
                "--service-status",
                "--debug",
            ],
        ),
        ("tui", &["-h", "--help", "--debug"]),
        ("purge", &["-h", "--help", "--debug"]),
    ];
    for (command, expected) in cases {
        let out = env.run([command, "--help"]);
        out.assert_code(0);
        assert!(
            out.stdout
                .starts_with(&format!("usage: {PROG} {command} [-h]")),
            "{}",
            out.stdout
        );
        assert_eq!(options(&out.stdout), *expected, "{command}");
    }
}
