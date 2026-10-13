//! argparse-compatible usage errors (exit 2, `usage:` line + `<prog>: error:`).
//!
//! Ported from upstream tests/test_cli.py::TestCLI::{test_no_args_shows_error,
//! test_mutually_exclusive_args, test_strategy_rejects_unknown_value,
//! test_token_status_flag_requires_list} and
//! TestJsonOutputCli::{test_json_rejected_without_supported_command,
//! test_token_status_with_json_rejected}.

mod support;

use support::{TestEnv, prog};

fn usage_error(env: &TestEnv, args: &[&str], message: &str) {
    let out = env.run(args);
    out.assert_code(2);
    assert_eq!(out.stdout, "", "{args:?}");
    let p = prog();
    assert_eq!(
        out.stderr,
        format!("usage: {p} <command> [args] [options]\n{p}: error: {message}\n"),
        "{args:?}"
    );
}

#[test]
fn argparse_errors_match_upstream() {
    let env = TestEnv::new();
    let p = prog();
    let no_command = format!("no command given — try '{p} help'");
    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec![], no_command.clone()),
        (vec!["--json"], no_command),
        (vec!["status", "--list"], "argument --list: not allowed with argument --status".into()),
        (vec!["--list", "status"], "unrecognized arguments: status".into()),
        (vec!["status", "extra"], "unrecognized arguments: extra".into()),
        (vec!["status", "--", "a"], "unrecognized arguments: -- a".into()),
        (vec!["bogus"], "unrecognized arguments: bogus".into()),
        (
            vec!["--s"],
            "ambiguous option: --s could match --strategy, --slot, --service-status, --switch, --switch-to, --status".into(),
        ),
        (
            vec!["--strategy", "x"],
            "argument --strategy: invalid choice: 'x' (choose from 'best', 'next-available')".into(),
        ),
        (vec!["--slot", "x", "status"], "argument --slot: invalid int value: 'x'".into()),
        (vec!["--slot"], "argument --slot: expected one argument".into()),
        (vec!["--status=1"], "argument --status: ignored explicit argument '1'".into()),
        (
            vec!["switch", "--switch-to"],
            "argument --switch-to: expected one argument".into(),
        ),
        (vec!["status", "--list=1"], "argument --list: ignored explicit argument '1'".into()),
        (
            vec!["status", "--token-status"],
            "--token-status can only be used with 'list'".into(),
        ),
        (
            vec!["list", "--json", "--token-status"],
            "--token-status cannot be combined with --json".into(),
        ),
        (
            vec!["add", "--json"],
            "--json can only be used with 'list', 'status', or 'switch'".into(),
        ),
    ];
    for (args, message) in cases {
        usage_error(&env, &args, &message);
    }
}

#[test]
fn version_wins_over_later_unrecognized_arguments() {
    let env = TestEnv::new();
    let out = env.run(["--version", "--bogus"]);
    out.assert_code(0);
    assert!(out.stdout.starts_with(&format!("{} ", prog())));
}

#[test]
fn help_lists_commands_and_hides_legacy_flags() {
    let env = TestEnv::new();
    let out = env.run(["help"]);
    out.assert_code(0);
    let p = prog();
    assert!(out.stdout.starts_with(&format!(
        "usage: {p} <command> [args] [options]\n\nMulti-Account Switcher for Claude Code\n"
    )));
    assert!(out.stdout.contains(&format!("  {p} status ")));
    let options = out
        .stdout
        .split("Flags combine with subcommands:")
        .next()
        .unwrap();
    assert!(!options.contains("--add-account"));
    assert!(!options.contains("--status"));
    assert!(
        out.stdout.ends_with("keep working.\n        \n"),
        "{:?}",
        out.stdout
    );
}

#[test]
fn empty_command_values_select_nothing() {
    let env = TestEnv::new();
    for args in [["--switch-to", ""], ["--export", ""], ["--import", ""]] {
        let out = env.run(args);
        out.assert_code(0);
        assert_eq!(
            (out.stdout.as_str(), out.stderr.as_str()),
            ("", ""),
            "{args:?}"
        );
    }
}
