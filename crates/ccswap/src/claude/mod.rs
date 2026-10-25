//! `ccswap claude <verb>`: the bare verbs under the Claude Code namespace.

pub mod accounts;
pub mod auto;
pub mod manage;
pub mod map;
pub mod openai;
pub mod renumber;
pub mod run;
pub mod unclaimed;

use crate::cli::namespace::Namespace;

const NAMESPACE: Namespace = Namespace {
    name: "claude",
    usage: "<command> [args] [options]",
    help: include_str!("help.txt"),
};

/// A Claude Code verb. One the global parser reads has the `flag` the bare
/// verb stands for and the `help` its `--help` prints before that parse; the
/// rest parse their own arguments.
pub struct Verb {
    pub names: &'static [&'static str],
    pub flag: Option<&'static str>,
    pub help: Option<&'static str>,
}

impl Verb {
    const fn global(
        names: &'static [&'static str],
        flag: &'static str,
        help: &'static str,
    ) -> Self {
        Verb {
            names,
            flag: Some(flag),
            help: Some(help),
        }
    }

    const fn own(names: &'static [&'static str]) -> Self {
        Verb {
            names,
            flag: None,
            help: None,
        }
    }
}

pub const VERBS: &[Verb] = &[
    Verb::global(&["add"], "--add-account", include_str!("help/add.txt")),
    Verb::global(
        &["add-token"],
        "--add-token",
        include_str!("help/add_token.txt"),
    ),
    Verb::global(&["list", "ls"], "--list", include_str!("help/list.txt")),
    Verb::global(&["status"], "--status", include_str!("help/status.txt")),
    // The global parser reads it as `--switch` or `--switch-to` by its first argument.
    Verb {
        names: &["switch"],
        flag: None,
        help: Some(include_str!("help/switch.txt")),
    },
    Verb::global(
        &["remove", "rm"],
        "--remove-account",
        include_str!("help/remove.txt"),
    ),
    Verb::global(
        &["disable"],
        "--disable-account",
        include_str!("help/disable.txt"),
    ),
    Verb::global(
        &["enable"],
        "--enable-account",
        include_str!("help/enable.txt"),
    ),
    Verb::own(&["alias"]),
    Verb::own(&["swap"]),
    Verb::own(&["move"]),
    Verb::own(&["run"]),
    Verb::own(&["map"]),
    Verb::own(&["unmap"]),
    Verb::own(&["auto"]),
    Verb::own(&["unclaimed"]),
    Verb::own(&["openai"]),
    Verb::global(&["export"], "--export", include_str!("help/export.txt")),
    Verb::global(&["import"], "--import", include_str!("help/import.txt")),
];

pub fn find(verb: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|v| v.names.contains(&verb))
}

/// The bare-verb arguments `claude <verb> ...` stands for. `Err` holds the
/// exit code once help or a usage error has been printed.
pub fn resolve(prog: &str, argv: &[String]) -> Result<Vec<String>, i32> {
    NAMESPACE.verb(prog, argv, find)?;
    Ok(argv.to_vec())
}
