mod support;

use support::{PROG, TestEnv};

#[test]
fn main_help_lists_the_codex_commands() {
    let env = TestEnv::new();
    let out = env.run(["help"]);
    out.assert_code(0);
    for line in [
        format!("  {PROG} codex add "),
        format!("  {PROG} codex login "),
        format!("  {PROG} codex list "),
        format!("  {PROG} codex switch "),
        format!("  {PROG} codex remove "),
        format!("  {PROG} codex alias "),
        format!("  {PROG} codex --help "),
    ] {
        assert!(out.stdout.contains(&line), "{line:?} in\n{}", out.stdout);
    }
}

#[test]
fn codex_help_lists_its_commands() {
    let env = TestEnv::new();
    for args in [
        vec!["codex", "--help"],
        vec!["codex", "-h"],
        vec!["codex", "help"],
    ] {
        let out = env.run(&args);
        out.assert_code(0);
        assert!(
            out.stdout
                .starts_with(&format!("usage: {PROG} codex <command> [options]\n")),
            "{args:?}: {}",
            out.stdout
        );
        assert!(out.stdout.contains(&format!("  {PROG} codex add ")));
        assert!(out.stdout.contains(&format!("  {PROG} codex login ")));
        assert!(out.stdout.contains(&format!("  {PROG} codex list ")));
        assert!(out.stdout.contains(&format!("  {PROG} codex switch ")));
        assert!(out.stdout.contains(&format!("  {PROG} codex remove ")));
        assert!(out.stdout.contains(&format!("  {PROG} codex alias ")));
    }
}

#[test]
fn every_help_names_a_codex_alias_target_as_its_verb_help_does() {
    let env = TestEnv::new();
    for args in [vec!["help"], vec!["codex", "--help"]] {
        let out = env.run(&args);
        out.assert_code(0);
        let alias_lines: Vec<&str> = out
            .stdout
            .lines()
            .filter(|line| line.starts_with(&format!("  {PROG} codex alias ")))
            .collect();
        assert!(!alias_lines.is_empty(), "{args:?}: {}", out.stdout);
        for line in alias_lines {
            assert!(
                line.starts_with(&format!("  {PROG} codex alias <num|alias|email> ")),
                "{args:?}: {line}"
            );
        }
    }
}

/// The option names in a help's `options:` section.
fn options(help: &str) -> Vec<&str> {
    help.lines()
        .skip_while(|line| *line != "options:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter(|line| line.starts_with("  -"))
        .flat_map(|line| {
            line.split_whitespace()
                .take_while(|word| word.starts_with('-'))
                .map(|word| word.trim_end_matches(','))
        })
        .collect()
}

#[test]
fn each_codex_verb_help_lists_only_its_own_options() {
    let cases: &[(&[&str], &[&str])] = &[
        (&["add"], &["-h", "--help", "--debug"]),
        (&["login"], &["-h", "--help", "--debug"]),
        (&["list"], &["-h", "--help", "--json", "--debug"]),
        (&["ls"], &["-h", "--help", "--json", "--debug"]),
        (&["switch", "2"], &["-h", "--help", "--json", "--debug"]),
        (&["remove"], &["-h", "--help", "-y", "--yes", "--debug"]),
        (&["rm", "2"], &["-h", "--help", "-y", "--yes", "--debug"]),
        (&["alias", "2"], &["-h", "--help", "--unset", "--debug"]),
    ];
    let env = TestEnv::new();
    for (args, expected) in cases {
        let verb = match args[0] {
            "ls" => "list",
            "rm" => "remove",
            verb => verb,
        };
        for flag in ["--help", "-h"] {
            let out = env.run(["codex"].iter().chain(*args).chain([&flag]));
            out.assert_code(0);
            assert!(
                out.stdout
                    .starts_with(&format!("usage: {PROG} codex {verb} [-h]")),
                "{args:?} {flag}: {}",
                out.stdout
            );
            assert_eq!(options(&out.stdout), *expected, "{args:?} {flag}");
        }
    }
}

#[test]
fn codex_without_a_known_command_is_a_usage_error() {
    let env = TestEnv::new();
    let out = env.run(["codex"]);
    out.assert_code(2);
    assert!(out.stderr.contains("no command given"), "{}", out.stderr);

    let out = env.run(["codex", "frobnicate"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains("unknown command 'frobnicate'"),
        "{}",
        out.stderr
    );

    let out = env.run(["codex", "add", "extra"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains("unrecognized arguments: extra"),
        "{}",
        out.stderr
    );

    let out = env.run(["codex", "add", "--json"]);
    out.assert_code(2);
    assert!(
        out.stderr.contains("unrecognized arguments: --json"),
        "{}",
        out.stderr
    );
}
