//! `ccswap swap A B` and `ccswap move ACCOUNT SLOT`, each with the argparse
//! surface of upstream's dedicated parser (two positionals and `--debug`).

use ccswap_core::accounts::{Accounts, Console};
use ccswap_core::anthropic_client::AnthropicClient;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::Error;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use ccswap_core::secret_store::{Keychain, SecretStore};
use ccswap_core::store;

use crate::cli::TerminalConsole;
use crate::config_cmd::{looks_like_option, lookup, split_explicit};

const OPTS: &[&str] = &["-h", "--help", "--debug"];

struct Spec {
    positionals: [&'static str; 2],
    help: &'static str,
}

fn spec(verb: &str) -> Spec {
    if verb == "swap" {
        Spec {
            positionals: ["NUM|EMAIL|ALIAS", "NUM|EMAIL|ALIAS"],
            help: include_str!("swap_help.txt"),
        }
    } else {
        Spec {
            positionals: ["NUM|EMAIL|ALIAS", "SLOT"],
            help: include_str!("move_help.txt"),
        }
    }
}

enum Exit {
    Help,
    Usage(String),
}

pub fn run(prog: &str, env: &Environment, verb: &str, argv: &[String]) -> i32 {
    let spec = spec(verb);
    let usage = format!(
        "{prog} {verb} [-h] [--debug] {} {}",
        spec.positionals[0], spec.positionals[1]
    );
    let positionals = match parse(&spec, argv) {
        Ok(positionals) => positionals,
        Err(Exit::Help) => {
            print!("{}", spec.help.replace("{prog}", prog));
            return 0;
        }
        Err(Exit::Usage(msg)) => {
            eprintln!("usage: {usage}");
            eprintln!("{prog} {verb}: error: {msg}");
            return 2;
        }
    };
    let paths = Paths::new(env);
    let style = Style::detect(env, &paths);
    if is_root() {
        eprintln!(
            "{}",
            style.red("Error: Do not run this script as root (unless running in a container)")
        );
        return 1;
    }
    crate::cli::run_store_migrations(env, &paths);
    let endpoints = Endpoints::new(env);
    let secrets = SecretStore::new(
        Keychain::new(endpoints.security.clone(), env),
        paths.clone(),
        env,
    );
    let client = AnthropicClient::new(endpoints);
    let accounts = Accounts {
        paths: &paths,
        env,
        secrets: &secrets,
        client: &client,
        style: &style,
    };
    let (first, second) = (positionals[0].as_str(), positionals[1].as_str());
    let mut console = TerminalConsole;
    let result = if verb == "swap" {
        swap(&accounts, &style, first, second, &mut console)
    } else {
        move_account(&accounts, &style, first, second, &mut console)
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{}", style.red(&format!("Error: {error}")));
            1
        }
    }
}

fn swap(
    accounts: &Accounts,
    style: &Style,
    first: &str,
    second: &str,
    console: &mut dyn Console,
) -> Result<(), Error> {
    let (num_a, num_b) = accounts.swap_accounts(first, second, console)?;
    print_swapped(accounts.paths, style, &num_a, &num_b, console);
    Ok(())
}

fn move_account(
    accounts: &Accounts,
    style: &Style,
    account: &str,
    slot: &str,
    console: &mut dyn Console,
) -> Result<(), Error> {
    let outcome = accounts.move_account(account, slot, console)?;
    let email = slot_email(accounts.paths, &outcome.target);
    if outcome.source == outcome.target {
        console.line(&format!(
            "{} slot {}: {email}",
            style.dimmed("Already in"),
            outcome.target
        ));
    } else if outcome.swapped {
        print_swapped(
            accounts.paths,
            style,
            &outcome.source,
            &outcome.target,
            console,
        );
    } else {
        console.line(&format!(
            "{} {email} to slot {}",
            style.accent("Moved"),
            outcome.target
        ));
    }
    Ok(())
}

fn print_swapped(paths: &Paths, style: &Style, a: &str, b: &str, console: &mut dyn Console) {
    console.line(&format!(
        "{} Account {a} and Account {b}:",
        style.accent("Swapped")
    ));
    let mut nums = [a, b];
    nums.sort_by_key(|n| n.parse::<u64>().unwrap_or_default());
    for num in nums {
        console.line(&format!("  {num}: {}", slot_email(paths, num)));
    }
}

fn slot_email(paths: &Paths, num: &str) -> String {
    store::read_sequence(paths)
        .ok()
        .flatten()
        .and_then(|data| {
            store::accounts(&data)?
                .get(num)?
                .get("email")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn parse(spec: &Spec, argv: &[String]) -> Result<Vec<String>, Exit> {
    let mut positionals = Vec::new();
    let mut extras: Vec<String> = Vec::new();
    let mut options_done = false;
    for arg in argv {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        if !options_done && looks_like_option(arg, OPTS) {
            let (name, explicit) = split_explicit(arg);
            match lookup(name, OPTS) {
                Some("--debug") => {
                    if let Some(v) = explicit {
                        return Err(Exit::Usage(format!(
                            "argument --debug: ignored explicit argument '{v}'"
                        )));
                    }
                }
                Some(_) => return Err(Exit::Help),
                None => extras.push(arg.clone()),
            }
        } else if positionals.len() < spec.positionals.len() {
            positionals.push(arg.clone());
        } else {
            extras.push(arg.clone());
        }
    }
    let missing = &spec.positionals[positionals.len()..];
    if !missing.is_empty() {
        return Err(Exit::Usage(format!(
            "the following arguments are required: {}",
            missing.join(", ")
        )));
    }
    if !extras.is_empty() {
        return Err(Exit::Usage(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    Ok(positionals)
}
