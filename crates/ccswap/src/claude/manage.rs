use ccswap_claude::accounts::Accounts;
use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::Error;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;

use crate::cli::TerminalConsole;
use crate::cli::argparse::Parsed;
use crate::cli::argv::{looks_like_option, lookup, split_explicit};

pub fn dispatch(
    env: &Environment,
    paths: &Paths,
    style: &Style,
    parsed: &Parsed,
) -> Option<Result<(), Error>> {
    let (identifier, disabled) = if let Some(identifier) = parsed.value("--remove-account") {
        (identifier, None)
    } else if let Some(identifier) = parsed.value("--disable-account") {
        (identifier, Some(true))
    } else {
        (parsed.value("--enable-account")?, Some(false))
    };
    let endpoints = Endpoints::new(env);
    let keychain = Keychain::new(endpoints.security.clone(), env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    let client = AnthropicClient::new(endpoints);
    let accounts = Accounts {
        paths,
        env,
        secrets: &secrets,
        client: &client,
        style,
    };
    let console = &mut TerminalConsole;
    Some(match disabled {
        Some(disabled) => accounts.set_account_disabled(identifier, disabled, console),
        // Upstream tests `args.remove_account` for truthiness: "" does nothing.
        None if identifier.is_empty() => Ok(()),
        None => {
            let assume_yes = parsed.flag("-y") || parsed.flag("--yes");
            accounts.remove_account(identifier, assume_yes, console)
        }
    })
}

const ALIAS_HELP: &str = include_str!("help/alias.txt");
const ALIAS_OPTS: &[&str] = &["-h", "--help", "--unset", "--debug"];

pub fn run_alias(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let usage_error = |msg: &str| {
        eprintln!("usage: {prog} alias [-h] [--unset] [--debug] [NUM|EMAIL] [NAME]");
        eprintln!("{prog} alias: error: {msg}");
        2
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut extras: Vec<String> = Vec::new();
    let mut unset = false;
    let mut debug = false;
    let mut options_done = false;
    for arg in argv {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        if !options_done && looks_like_option(arg, ALIAS_OPTS) {
            let (name, explicit) = split_explicit(arg);
            match lookup(name, ALIAS_OPTS) {
                Some(opt @ ("--unset" | "--debug")) => {
                    if let Some(v) = explicit {
                        return usage_error(&format!(
                            "argument {opt}: ignored explicit argument '{v}'"
                        ));
                    }
                    unset |= opt == "--unset";
                    debug |= opt == "--debug";
                }
                Some(_) => {
                    print!("{}", ALIAS_HELP.replace("{prog}", prog));
                    return 0;
                }
                None => extras.push(arg.clone()),
            }
        } else if positionals.len() < 2 {
            positionals.push(arg.clone());
        } else {
            extras.push(arg.clone());
        }
    }
    if !extras.is_empty() {
        return usage_error(&format!("unrecognized arguments: {}", extras.join(" ")));
    }
    let account = positionals.first();
    let name = positionals.get(1).filter(|n| !n.is_empty());
    if unset && name.is_some() {
        return usage_error("--unset does not take a NAME argument");
    }
    if unset && account.is_none() {
        return usage_error("NUM|EMAIL is required with --unset");
    }
    if account.is_some() && !unset && name.is_none() {
        return usage_error("NAME is required (or pass --unset to remove the alias)");
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
    crate::cli::run_store_migrations(env, &paths, debug);
    let endpoints = Endpoints::new(env);
    let keychain = Keychain::new(endpoints.security.clone(), env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    let client = AnthropicClient::new(endpoints);
    let accounts = Accounts {
        paths: &paths,
        env,
        secrets: &secrets,
        client: &client,
        style: &style,
    };
    let result = match (account, name) {
        (None, _) => accounts.list_aliases().map(|rows| {
            if rows.is_empty() {
                println!("{}", style.dimmed("No aliases set"));
                return;
            }
            println!("{}", style.bolded("Aliases:"));
            for (num, alias, email) in rows {
                println!("  {num}: {alias} {}", style.muted(&format!("({email})")));
            }
        }),
        (Some(account), None) => accounts.unset_alias(account).map(|num| {
            println!("{} for Account {num}", style.accent("Removed alias"));
        }),
        (Some(account), Some(name)) => {
            accounts.set_alias(account, name).map(|(num, normalized)| {
                println!(
                    "{} '{normalized}' for Account {num}",
                    style.accent("Set alias")
                );
            })
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{}", style.red(&format!("Error: {error}")));
            1
        }
    }
}
