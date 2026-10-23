//! `ccswap codex <command>`. Never touches the Claude store, Keychain or its migrations.

use ccswap_core::codex::{CodexPaths, CodexStore, view};
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::Error;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;

use crate::argparse::{Action, Arity, Opt, Outcome, ParseError, Parsed, Parser};

const HELP: &str = include_str!("codex_help.txt");

const HELP_OPTS: [Opt; 3] = [
    Opt::flag("-h").action(Action::Help),
    Opt::flag("--help").action(Action::Help),
    Opt::flag("--debug"),
];

/// Gets the parsed options and positionals; returns the stdout lines.
type Handler = fn(&Context, &Parsed, &[String]) -> Result<Vec<String>, Error>;

struct Command {
    names: &'static [&'static str],
    /// Beyond `-h`, `--help` and `--debug`.
    opts: &'static [Opt],
    max_positionals: usize,
    run: Handler,
}

const COMMANDS: &[Command] = &[
    Command {
        names: &["add"],
        opts: &[],
        max_positionals: 0,
        run: add,
    },
    Command {
        names: &["list", "ls"],
        opts: &[Opt::flag("--json")],
        max_positionals: 0,
        run: list,
    },
];

struct Context {
    store: CodexStore,
    style: Style,
}

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let usage_error = |msg: &str| {
        eprintln!("usage: {prog} codex <command> [options]");
        eprintln!("{prog} codex: error: {msg}");
        2
    };
    let print_help = || {
        print!("{}", HELP.replace("{prog}", prog));
        0
    };
    let Some((verb, rest)) = argv.split_first() else {
        return usage_error(&format!("no command given — try '{prog} codex --help'"));
    };
    if matches!(verb.as_str(), "-h" | "--help" | "help") {
        return print_help();
    }
    let Some(command) = COMMANDS.iter().find(|c| c.names.contains(&verb.as_str())) else {
        return usage_error(&format!(
            "unknown command '{verb}' — try '{prog} codex --help'"
        ));
    };
    let opts: Vec<Opt> = HELP_OPTS.iter().chain(command.opts).copied().collect();
    let (positionals, options) = split_positionals(rest, &opts);
    let parsed = match (Parser { opts: &opts }).parse(&options) {
        Ok((Outcome::Help, _)) => return print_help(),
        Ok((Outcome::Version, _)) => unreachable!("codex commands take no --version"),
        Ok((Outcome::Parsed, parsed)) => parsed,
        Err(ParseError(msg)) => return usage_error(&msg),
    };
    if positionals.len() > command.max_positionals {
        let extra = &positionals[command.max_positionals..];
        return usage_error(&format!("unrecognized arguments: {}", extra.join(" ")));
    }

    let paths = Paths::new(env);
    let style = Style::detect(env, &paths);
    if is_root() {
        eprintln!(
            "{}",
            style.red("Error: Do not run this script as root (unless running in a container)")
        );
        return 1;
    }
    ccswap_core::log::setup(&paths.backup_root(), parsed.flag("--debug"));
    let context = Context {
        store: CodexStore::new(CodexPaths::new(env)),
        style,
    };
    match (command.run)(&context, &parsed, &positionals) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            0
        }
        Err(error) => {
            if parsed.flag("--json") {
                println!(
                    "{}",
                    pyjson::to_string_pretty(&presentation::error_envelope(&error))
                );
            } else {
                eprintln!("{}", context.style.red(&format!("Error: {error}")));
            }
            1
        }
    }
}

/// Bare words are positionals unless they are the value of a value-taking option.
fn split_positionals(args: &[String], opts: &[Opt]) -> (Vec<String>, Vec<String>) {
    let (mut positionals, mut options) = (Vec::new(), Vec::new());
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            positionals.extend(args.by_ref().cloned());
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            positionals.push(arg.clone());
            continue;
        }
        options.push(arg.clone());
        let takes_value = opts
            .iter()
            .any(|o| o.name == arg && o.arity == Arity::Value);
        if takes_value && let Some(value) = args.next() {
            options.push(value.clone());
        }
    }
    (positionals, options)
}

fn add(context: &Context, _: &Parsed, _: &[String]) -> Result<Vec<String>, Error> {
    let outcome = context.store.add_live()?;
    Ok(view::add_lines(&outcome, &context.style))
}

fn list(context: &Context, parsed: &Parsed, _: &[String]) -> Result<Vec<String>, Error> {
    let listing = context.store.list()?;
    if parsed.flag("--json") {
        return Ok(vec![pyjson::to_string_pretty(&view::list_json(&listing))]);
    }
    Ok(view::list_lines(&listing, &context.style))
}
