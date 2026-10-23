//! `ccswap claude <verb>`: the bare verbs under the Claude Code namespace.

pub mod accounts;
pub mod auto;
pub mod manage;
pub mod map;
pub mod renumber;
pub mod run;
pub mod unclaimed;

use crate::cli::VerbHelp;

const HELP: &str = include_str!("help.txt");

pub const VERB_HELP: VerbHelp = &[
    (&["list", "ls"], include_str!("help/list.txt")),
    (&["status"], include_str!("help/status.txt")),
    (&["switch"], include_str!("help/switch.txt")),
    (&["add"], include_str!("help/add.txt")),
    (&["add-token"], include_str!("help/add_token.txt")),
    (&["remove", "rm"], include_str!("help/remove.txt")),
    (&["disable"], include_str!("help/disable.txt")),
    (&["enable"], include_str!("help/enable.txt")),
    (&["export"], include_str!("help/export.txt")),
    (&["import"], include_str!("help/import.txt")),
];

const VERBS: &[&str] = &[
    "add",
    "add-token",
    "list",
    "ls",
    "status",
    "switch",
    "remove",
    "rm",
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

pub enum Resolved {
    /// The bare-verb arguments `claude <verb> ...` stands for.
    Bare(Vec<String>),
    /// Help or a usage error was printed; exit with this code.
    Exit(i32),
}

pub fn resolve(prog: &str, argv: &[String]) -> Resolved {
    let usage_error = |msg: &str| {
        eprintln!("usage: {prog} claude <command> [args] [options]");
        eprintln!("{prog} claude: error: {msg}");
        Resolved::Exit(2)
    };
    let Some(verb) = argv.first() else {
        return usage_error(&format!("no command given — try '{prog} claude --help'"));
    };
    if matches!(verb.as_str(), "-h" | "--help" | "help") {
        print!("{}", HELP.replace("{prog}", prog));
        return Resolved::Exit(0);
    }
    if !VERBS.contains(&verb.as_str()) {
        return usage_error(&format!(
            "unknown command '{verb}' — try '{prog} claude --help'"
        ));
    }
    Resolved::Bare(argv.to_vec())
}
