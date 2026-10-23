//! `ccswap claude <verb>`: the bare verbs under the Claude Code namespace.

const HELP: &str = include_str!("claude_help.txt");

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
