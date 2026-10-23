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
fn codex_help_lists_its_commands_from_any_position() {
    let env = TestEnv::new();
    for args in [
        vec!["codex", "--help"],
        vec!["codex", "-h"],
        vec!["codex", "help"],
        vec!["codex", "list", "--help"],
        vec!["codex", "remove", "--help"],
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
