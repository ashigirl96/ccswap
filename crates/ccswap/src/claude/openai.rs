use ccswap_claude::openai::InferenceLogins;
use ccswap_codex::{CodexPaths, Ledger};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{Error, Result};
use ccswap_core::paths::Paths;

const HELP: &str = "usage: {prog} claude openai <command>\n\nCommands:\n  login <number>   Continue with ChatGPT for a Codex account\n  logout <number>  remove that account's inference login\n  status           show inference logins for Codex accounts\n";

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", HELP.replace("{prog}", prog));
        return 0;
    }
    if is_root() {
        eprintln!("Error: Do not run this script as root (unless running in a container)");
        return 1;
    }
    match execute(env, argv) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

fn execute(env: &Environment, argv: &[String]) -> Result<()> {
    let ledger = Ledger::load(&CodexPaths::new(env).ledger_file())?;
    let logins = InferenceLogins::new(&Paths::new(env));
    match argv
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["status"] => {
            for (number, account) in ledger.ordered() {
                let present = logins
                    .read(&account.account_id)?
                    .is_some_and(|v| v["access_token"].as_str().is_some_and(|t| !t.is_empty()));
                println!(
                    "{number}  {}  {}",
                    account.email,
                    if present {
                        "logged in"
                    } else {
                        "not logged in"
                    }
                );
            }
            Ok(())
        }
        [verb @ ("login" | "logout"), number] => {
            let account = number
                .parse::<u32>()
                .ok()
                .and_then(|n| ledger.accounts.get(&n))
                .ok_or_else(|| Error::config(format!("No Codex account {number}")))?;
            if account.account_id.is_empty() {
                return Err(Error::config("The Codex account has no account ID"));
            }
            if *verb == "login" {
                logins.login(&account.account_id, &account.email, &Endpoints::new(env))?;
                println!("Saved inference login for Codex account {number}");
            } else {
                logins.logout(&account.account_id)?;
                println!("Removed inference login for Codex account {number}");
            }
            Ok(())
        }
        _ => Err(Error::config(
            "Expected 'ccswap claude openai login <number>', 'logout <number>' or 'status'",
        )),
    }
}
