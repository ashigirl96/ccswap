//! `ccswap codex <command>`. Never touches the Claude store, Keychain or its migrations.

use ccswap_core::codex::{
    CodexPaths, CodexStore, CodexUsage, LoginStdio, identifier_candidates, resolve_account, view,
};
use ccswap_core::console::Console;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{Error, ErrorKind};
use ccswap_core::identifier::{self, Picker};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;

use crate::argparse::{Action, Arity, Opt, Outcome, ParseError, Parsed, Parser};
use crate::cli::TerminalConsole;

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
        names: &["login"],
        opts: &[],
        max_positionals: 0,
        run: login,
    },
    Command {
        names: &["list", "ls"],
        opts: &[Opt::flag("--json")],
        max_positionals: 0,
        run: list,
    },
    Command {
        names: &["switch"],
        opts: &[Opt::flag("--json")],
        max_positionals: 1,
        run: switch,
    },
    Command {
        names: &["remove", "rm"],
        opts: &[Opt::flag("-y"), Opt::flag("--yes")],
        max_positionals: 1,
        run: remove,
    },
    Command {
        names: &["alias"],
        opts: &[],
        max_positionals: 2,
        run: alias,
    },
];

struct Context {
    store: CodexStore,
    usage: CodexUsage,
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
    let codex_paths = CodexPaths::new(env);
    let context = Context {
        usage: CodexUsage::new(&codex_paths, &Endpoints::new(env)),
        store: CodexStore::new(codex_paths),
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

fn remove(context: &Context, parsed: &Parsed, args: &[String]) -> Result<Vec<String>, Error> {
    let [identifier] = args else {
        return Err(Error::new(
            ErrorKind::ValidationError,
            "remove needs an account number or email",
        ));
    };
    let style = &context.style;
    let mut console = TerminalConsole;
    let ledger = context.store.ledger()?;
    let picker = Picker {
        console: &mut console,
        style,
        action: "remove",
    };
    let Some(identifier) =
        identifier::screen(&identifier_candidates(&ledger), identifier, Some(picker))?
    else {
        return Ok(Vec::new());
    };
    let number = resolve_account(&ledger, &identifier)?;
    let listed = context
        .store
        .list()?
        .accounts
        .into_iter()
        .find(|a| a.number == number);
    let email = listed.as_ref().map_or("", |a| a.email.as_str());
    if listed.as_ref().is_some_and(|a| a.active) {
        console.line(&style.yellowed(&format!(
            "Warning: Codex account {number} ({email}) is currently logged in. The live login stays; only ccswap's copy is removed."
        )));
    }
    if !(parsed.flag("-y") || parsed.flag("--yes")) {
        let question =
            format!("Are you sure you want to remove Codex account {number} ({email})? [y/N] ");
        match console.prompt(&question) {
            Some(answer) if answer.trim().eq_ignore_ascii_case("y") => {}
            Some(_) => return Ok(vec![style.dimmed("Cancelled")]),
            None => return Ok(vec![format!("\n{}", style.dimmed("Cancelled"))]),
        }
    }
    let (number, account) = context.store.remove(&number.to_string())?;
    Ok(vec![format!(
        "{} {number} ({})",
        style.accent("Removed Codex account"),
        account.email
    )])
}

fn alias(context: &Context, _: &Parsed, args: &[String]) -> Result<Vec<String>, Error> {
    let [identifier, name] = args else {
        return Err(Error::new(
            ErrorKind::ValidationError,
            "alias needs an account number or email and a name (\"\" removes the alias)",
        ));
    };
    let (number, alias) = context.store.set_alias(identifier, name)?;
    Ok(vec![match alias {
        Some(alias) => format!(
            "{} Codex account {number} as {}",
            context.style.accent("Aliased"),
            context.style.accent(&alias)
        ),
        None => format!(
            "{} the alias of Codex account {number}",
            context.style.accent("Removed")
        ),
    }])
}

fn login(context: &Context, _: &Parsed, _: &[String]) -> Result<Vec<String>, Error> {
    survive_ctrl_c();
    let outcome = context.store.login(LoginStdio::Inherit)?;
    Ok(view::login_lines(&outcome, &context.style))
}

/// Ctrl-C still stops `codex login`, and ccswap lives on to remove the new home.
fn survive_ctrl_c() {
    // A handler, unlike SIG_IGN, resets to the default in the exec'd child.
    extern "C" fn ignore(_: libc::c_int) {}
    let handler: extern "C" fn(libc::c_int) = ignore;
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

fn list(context: &Context, parsed: &Parsed, _: &[String]) -> Result<Vec<String>, Error> {
    let listing = context.store.list()?;
    let usage = context.usage.collect(&context.store, &listing);
    if parsed.flag("--json") {
        return Ok(vec![pyjson::to_string_pretty(&view::list_json(
            &listing, &usage,
        ))]);
    }
    Ok(view::list_lines(&listing, &usage, &context.style))
}

fn switch(
    context: &Context,
    parsed: &Parsed,
    positionals: &[String],
) -> Result<Vec<String>, Error> {
    let identifier = match positionals.first() {
        None => None,
        Some(identifier) => {
            let mut console = TerminalConsole;
            let picker = (!parsed.flag("--json")).then_some(Picker {
                console: &mut console,
                style: &context.style,
                action: "switch to",
            });
            let candidates = identifier_candidates(&context.store.ledger()?);
            match identifier::screen(&candidates, identifier, picker)? {
                Some(identifier) => Some(identifier),
                None => return Ok(Vec::new()),
            }
        }
    };
    let outcome = context.store.switch(identifier.as_deref())?;
    if parsed.flag("--json") {
        return Ok(vec![pyjson::to_string_pretty(&view::switch_json(&outcome))]);
    }
    Ok(view::switch_lines(&outcome, &context.style))
}
