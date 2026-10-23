//! Reading `--help` output: command groups and option lists.

use super::PROG;

pub const CLAUDE_VERBS: [&str; 18] = [
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

pub const CODEX_VERBS: [&str; 6] = ["add", "login", "list", "switch", "remove", "alias"];

/// The command lines between `heading` and the next blank line.
pub fn group<'a>(help: &'a str, heading: &str) -> Vec<&'a str> {
    help.lines()
        .skip_while(|line| !line.starts_with(heading))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .collect()
}

pub fn lists(lines: &[&str], command: &str) -> bool {
    lines.iter().any(|line| {
        line.starts_with(&format!("  {PROG} {command} ")) || *line == format!("  {PROG} {command}")
    })
}

/// The option names in a help's `options:` section.
pub fn options(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| *line != "options:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter(|line| line.starts_with("  -"))
        .flat_map(|line| {
            line.split_whitespace()
                .take_while(|word| word.starts_with('-'))
                .map(|word| word.trim_end_matches(',').to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}
