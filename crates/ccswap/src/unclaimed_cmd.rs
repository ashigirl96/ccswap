use ccswap_claude::unclaimed::UnclaimedStash;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use serde_json::Value;

use crate::config_cmd::{looks_like_option, lookup, split_explicit};

const HELP: &str = include_str!("unclaimed_help.txt");
const OPTS: &[&str] = &["-h", "--help", "--purge", "--debug"];

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let usage_error = |msg: &str| {
        eprintln!("usage: {prog} unclaimed [-h] [--purge ID] [--debug]");
        eprintln!("{prog} unclaimed: error: {msg}");
        2
    };
    let mut purge: Option<String> = None;
    let mut debug = false;
    let mut extras: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        if options_done || !looks_like_option(arg, OPTS) {
            extras.push(arg.clone());
            continue;
        }
        let (name, explicit) = split_explicit(arg);
        match lookup(name, OPTS) {
            Some("--purge") => match explicit {
                Some(value) => purge = Some(value.to_string()),
                None => match args.next().filter(|v| !looks_like_option(v, OPTS)) {
                    Some(value) => purge = Some(value.clone()),
                    None => return usage_error("argument --purge: expected one argument"),
                },
            },
            Some("--debug") => {
                if let Some(v) = explicit {
                    return usage_error(&format!(
                        "argument --debug: ignored explicit argument '{v}'"
                    ));
                }
                debug = true;
            }
            Some(_) => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            None => extras.push(arg.clone()),
        }
    }
    if !extras.is_empty() {
        return usage_error(&format!("unrecognized arguments: {}", extras.join(" ")));
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
    let stash = UnclaimedStash::new(&paths);
    let entries = stash.list();

    if let Some(id) = purge.filter(|id| !id.is_empty()) {
        if !entries.contains_key(&id) {
            eprintln!("{}", style.red(&format!("Error: no unclaimed entry {id}")));
            return 1;
        }
        if let Err(error) = stash.remove(&id) {
            eprintln!("{}", style.red(&format!("Error: {error}")));
            return 1;
        }
        println!("{} {id}", style.accent("Purged"));
        return 0;
    }

    if entries.is_empty() {
        println!("{}", style.dimmed("No unclaimed credential entries"));
        return 0;
    }
    for (id, meta) in &entries {
        let text = |key: &str| {
            meta.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let slot = text("configSlot").unwrap_or_else(|| "?".into());
        let reason = text("reason").unwrap_or_else(|| "orphaned (no manifest row)".into());
        println!("{id}  slot {slot}  {reason}");
    }
    0
}
