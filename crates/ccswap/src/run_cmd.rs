use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;

use ccswap_core::anthropic_client::AnthropicClient;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{self, Error};
use ccswap_core::mappings::{self, DirectoryAccount};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use ccswap_core::secret_store::{Keychain, SecretStore};
use ccswap_core::session::{Launch, RunOptions, Sessions};

use crate::cli::TerminalConsole;
use crate::config_cmd::split_explicit;

const HELP: &str = include_str!("run_help.txt");
const OPTS: &[&str] = &[
    "-h",
    "--help",
    "--no-share",
    "--share-history",
    "--no-share-history",
    "--require-session",
    "--debug",
];
const USAGE_OPTIONALS: &[&str] = &[
    "[-h]",
    "[--no-share]",
    "[--share-history | --no-share-history]",
    "[--require-session]",
    "[--debug]",
];
const USAGE_WIDTH: usize = 78;

struct Args {
    account: Option<String>,
    debug: bool,
    options: RunOptions,
}

enum Exit {
    Help,
    Usage(String),
}

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let (head, tail) = match argv.iter().position(|a| a == "--") {
        Some(split) => (&argv[..split], &argv[split + 1..]),
        None => (argv, &[][..]),
    };
    let args = match parse(head) {
        Ok(args) => args,
        Err(Exit::Help) => {
            print!(
                "{}",
                HELP.replace("{usage}", &usage(prog))
                    .replace("{prog}", prog)
            );
            return 0;
        }
        Err(Exit::Usage(msg)) => {
            eprintln!("{}", usage(prog));
            eprintln!("{prog} run: error: {msg}");
            return 2;
        }
    };

    let paths = Paths::new(env);
    let style = Style::detect(env, &paths);
    let fail = |error: &Error| {
        eprintln!("{}", style.red(&format!("Error: {error}")));
        1
    };
    if is_root() {
        eprintln!(
            "{}",
            style.red("Error: Do not run this script as root (unless running in a container)")
        );
        return 1;
    }
    crate::cli::run_store_migrations(env, &paths, args.debug);
    let endpoints = Endpoints::new(env);
    let secrets = SecretStore::new(
        Keychain::new(endpoints.security.clone(), env),
        paths.clone(),
        env,
    );
    let client = AnthropicClient::new(endpoints);
    let sessions = Sessions {
        paths: &paths,
        env,
        secrets: &secrets,
        client: &client,
        style: &style,
    };
    let account = match args.account {
        Some(account) => account,
        None => match mapped_account(env, &paths, &style) {
            Ok(Some(slot)) => slot,
            Ok(None) => {
                return match sessions.exec_default(tail) {
                    Ok(launch) => exec(&launch, &style),
                    Err(error) => fail(&error),
                };
            }
            Err(error) => return fail(&error),
        },
    };
    match sessions.run(&account, tail, args.options, &mut TerminalConsole) {
        Ok(launch) => exec(&launch, &style),
        Err(error) => fail(&error),
    }
}

/// The slot mapped to the working directory, or None (after saying why)
/// when the default account should launch instead.
fn mapped_account(
    env: &Environment,
    paths: &Paths,
    style: &Style,
) -> error::Result<Option<String>> {
    let cwd = crate::map_cmd::cwd();
    let key = mappings::normalize_path(Path::new(&cwd), env);
    Ok(match mappings::account_for_directory(paths, &key)? {
        DirectoryAccount::Mapped { slot, .. } => Some(slot),
        DirectoryAccount::Removed { email } => {
            println!(
                "{}",
                style.yellowed(&format!(
                    "Mapped account {email} no longer exists — launching the default account."
                ))
            );
            None
        }
        DirectoryAccount::Unmapped => {
            println!(
                "{}",
                style.dimmed(&format!(
                    "No account mapped for {cwd} — launching the default account."
                ))
            );
            None
        }
    })
}

/// Replaces this process; returns only when the exec itself failed.
fn exec(launch: &Launch, style: &Style) -> i32 {
    let _ = std::io::stdout().flush();
    let error = std::process::Command::new(&launch.program)
        .arg0(&launch.program)
        .args(&launch.args)
        .env_clear()
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .exec();
    eprintln!(
        "{}",
        style.red(&format!(
            "Error: could not launch {}: {error}",
            launch.program.display()
        ))
    );
    1
}

/// argparse semantics: unique long-option prefixes, errors in encounter
/// order, unrecognized arguments reported together at the end.
fn parse(argv: &[String]) -> Result<Args, Exit> {
    let mut args = Args {
        account: None,
        debug: false,
        options: RunOptions::default(),
    };
    let mut extras: Vec<String> = Vec::new();
    for arg in argv {
        let (name, explicit) = split_explicit(arg);
        let resolved = if arg.starts_with('-') && arg != "-" {
            resolve(name)?
        } else {
            None
        };
        if resolved.is_none() && is_positional(arg) {
            if args.account.is_none() {
                args.account = Some(arg.clone());
            } else {
                extras.push(arg.clone());
            }
            continue;
        }
        let opt = match resolved {
            Some(opt) => opt,
            None => {
                extras.push(arg.clone());
                continue;
            }
        };
        if let Some(value) = explicit {
            let shown = if opt.ends_with("share-history") {
                "--share-history/--no-share-history"
            } else {
                opt
            };
            return Err(Exit::Usage(format!(
                "argument {shown}: ignored explicit argument '{value}'"
            )));
        }
        match opt {
            "-h" | "--help" => return Err(Exit::Help),
            "--no-share" => args.options.share = false,
            "--share-history" => args.options.share_history = true,
            "--no-share-history" => args.options.share_history = false,
            "--require-session" => args.options.require_session = true,
            "--debug" => args.debug = true,
            _ => {}
        }
    }
    if !extras.is_empty() {
        return Err(Exit::Usage(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    Ok(args)
}

fn resolve(name: &str) -> Result<Option<&'static str>, Exit> {
    if let Some(opt) = OPTS.iter().find(|o| **o == name) {
        return Ok(Some(opt));
    }
    if !name.starts_with("--") || name.len() < 3 {
        return Ok(None);
    }
    let matches: Vec<&str> = OPTS
        .iter()
        .filter(|o| o.starts_with(name))
        .copied()
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => Err(Exit::Usage(format!(
            "ambiguous option: {name} could match {}",
            many.join(", ")
        ))),
    }
}

fn is_positional(arg: &str) -> bool {
    !arg.starts_with('-') || arg == "-" || is_negative_number(arg) || arg.contains(' ')
}

fn is_negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    match rest.split_once('.') {
        None => digits(rest),
        Some((int, frac)) => (int.is_empty() || digits(int)) && digits(frac),
    }
}

/// argparse's wrapped usage: optionals after the prog, then the positional
/// on its own line, both aligned under the first option.
fn usage(prog: &str) -> String {
    let prefix = "usage: ";
    let prog_part = format!("{prog} run");
    let indent = " ".repeat(prefix.len() + prog_part.len() + 1);
    let mut lines: Vec<String> = Vec::new();
    let mut line: Vec<&str> = Vec::new();
    let mut line_len = prefix.len() - 1;
    for part in std::iter::once(prog_part.as_str()).chain(USAGE_OPTIONALS.iter().copied()) {
        if line_len + 1 + part.len() > USAGE_WIDTH && !line.is_empty() {
            lines.push(line.join(" "));
            line.clear();
            line_len = indent.len() - 1;
        }
        line.push(part);
        line_len += part.len() + 1;
    }
    lines.push(line.join(" "));
    let mut out = format!("{prefix}{}", lines[0]);
    for rest in &lines[1..] {
        out.push_str(&format!("\n{indent}{rest}"));
    }
    out.push_str(&format!("\n{indent}[NUM|EMAIL]"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_wraps_like_argparse() {
        assert_eq!(
            usage("cswap"),
            "usage: cswap run [-h] [--no-share] [--share-history | --no-share-history]\n                 [--require-session] [--debug]\n                 [NUM|EMAIL]"
        );
    }
}
