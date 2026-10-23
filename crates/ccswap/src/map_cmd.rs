use std::path::Path;

use ccswap_claude::accounts::{Accounts, ResolvedAccount};
use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::mappings::{self, MappingStore};
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{Error, ErrorKind, Result};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;

use crate::config_cmd::{looks_like_option, lookup, split_explicit};

const MAP_HELP: &str = include_str!("map_help.txt");
const UNMAP_HELP: &str = include_str!("unmap_help.txt");
const OPTS: &[&str] = &["-h", "--help", "--debug"];

struct Args {
    positionals: Vec<String>,
    debug: bool,
}

enum Exit {
    Help,
    Usage(String),
}

pub fn run_map(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let args = match parse(argv, 2) {
        Ok(args) => args,
        Err(exit) => {
            return report_exit(
                exit,
                MAP_HELP,
                prog,
                &format!("{prog} map [-h] [--debug] [NUM|EMAIL] [PATH]"),
                &format!("{prog} map"),
            );
        }
    };
    let Some(ctx) = Context::start(env, args.debug) else {
        return 1;
    };
    let result = match args.positionals.first() {
        None => list(&ctx),
        Some(account) => map(&ctx, account, args.positionals.get(1)),
    };
    ctx.finish(result)
}

pub fn run_unmap(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let args = match parse(argv, 1) {
        Ok(args) => args,
        Err(exit) => {
            return report_exit(
                exit,
                UNMAP_HELP,
                prog,
                &format!("{prog} unmap [-h] [--debug] [PATH]"),
                &format!("{prog} unmap"),
            );
        }
    };
    let Some(ctx) = Context::start(env, args.debug) else {
        return 1;
    };
    let result = unmap(&ctx, args.positionals.first());
    ctx.finish(result)
}

struct Context<'a> {
    env: &'a Environment,
    paths: Paths,
    style: Style,
}

impl<'a> Context<'a> {
    /// Root guard and store migrations; None (after reporting) under root.
    fn start(env: &'a Environment, debug: bool) -> Option<Self> {
        let paths = Paths::new(env);
        let style = Style::detect(env, &paths);
        if is_root() {
            eprintln!(
                "{}",
                style.red("Error: Do not run this script as root (unless running in a container)")
            );
            return None;
        }
        crate::cli::run_store_migrations(env, &paths, debug);
        Some(Context { env, paths, style })
    }

    fn finish(&self, result: Result<()>) -> i32 {
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{}", self.style.red(&format!("Error: {error}")));
                1
            }
        }
    }

    fn store(&self) -> MappingStore {
        MappingStore::new(&self.paths.backup_root())
    }
}

fn target(path: Option<&String>) -> String {
    match path.filter(|p| !p.is_empty()) {
        Some(path) => path.clone(),
        None => cwd(),
    }
}

pub(crate) fn cwd() -> String {
    std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default()
}

fn io_error(error: std::io::Error) -> Error {
    Error::new(ErrorKind::ClaudeSwitchError, error.to_string())
}

fn map(ctx: &Context, account: &str, path: Option<&String>) -> Result<()> {
    let endpoints = Endpoints::new(ctx.env);
    let secrets = SecretStore::new(
        Keychain::new(endpoints.security.clone(), ctx.env),
        ctx.paths.clone(),
        ctx.env,
    );
    let client = AnthropicClient::new(endpoints);
    let accounts = Accounts {
        paths: &ctx.paths,
        env: ctx.env,
        secrets: &secrets,
        client: &client,
        style: &ctx.style,
    };
    let ResolvedAccount {
        number: num,
        email,
        organization_uuid: org,
    } = accounts.resolve_account(account)?;
    let target = target(path);
    if !Path::new(&target).is_dir() {
        println!(
            "{}",
            ctx.style.yellowed(&format!(
                "Warning: {target} is not an existing directory (mapping it anyway)"
            ))
        );
    }
    let key = mappings::normalize_path(Path::new(&target), ctx.env);
    let store = ctx.store();
    let previous = store.get(&key);
    store.set(&key, &email, &org).map_err(io_error)?;
    let mut line = format!(
        "{} {key} → Account-{num} ({email})",
        ctx.style.accent("Mapped")
    );
    let previous_email = previous
        .as_ref()
        .and_then(|p| p.get("email"))
        .and_then(serde_json::Value::as_str)
        .filter(|prev| *prev != email);
    if let Some(prev) = previous_email {
        line.push_str(&format!(" {}", ctx.style.muted(&format!("(was {prev})"))));
    }
    println!("{line}");
    Ok(())
}

fn list(ctx: &Context) -> Result<()> {
    let rows = mappings::mapping_rows(&ctx.paths)?;
    let style = &ctx.style;
    if rows.is_empty() {
        println!("{}", style.dimmed("No directory mappings yet."));
        println!(
            "{}",
            style.muted("Map one with: ccswap map <NUM|EMAIL> [PATH]")
        );
        return Ok(());
    }
    println!("{}", style.bolded("Directory mappings:"));
    for row in rows {
        let arrow = style.dimmed("→");
        match row.account {
            Some((slot, tag)) => println!(
                "  {} {arrow} {slot}: {} {}",
                row.path,
                row.email,
                style.muted(&format!("[{tag}]"))
            ),
            None => println!(
                "  {} {arrow} {} {}",
                row.path,
                row.email,
                style.muted("(account removed)")
            ),
        }
    }
    Ok(())
}

fn unmap(ctx: &Context, path: Option<&String>) -> Result<()> {
    let key = mappings::normalize_path(Path::new(&target(path)), ctx.env);
    if ctx.store().remove(&key).map_err(io_error)? {
        println!("{} {key}", ctx.style.accent("Unmapped"));
    } else {
        println!("{}", ctx.style.dimmed(&format!("No mapping for {key}")));
    }
    Ok(())
}

fn report_exit(exit: Exit, help: &str, prog: &str, usage: &str, name: &str) -> i32 {
    match exit {
        Exit::Help => {
            print!("{}", help.replace("{prog}", prog));
            0
        }
        Exit::Usage(msg) => {
            eprintln!("usage: {usage}");
            eprintln!("{name}: error: {msg}");
            2
        }
    }
}

fn parse(argv: &[String], max: usize) -> std::result::Result<Args, Exit> {
    let mut args = Args {
        positionals: Vec::new(),
        debug: false,
    };
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
                    args.debug = true;
                }
                Some(_) => return Err(Exit::Help),
                None => extras.push(arg.clone()),
            }
        } else if args.positionals.len() < max {
            args.positionals.push(arg.clone());
        } else {
            extras.push(arg.clone());
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
