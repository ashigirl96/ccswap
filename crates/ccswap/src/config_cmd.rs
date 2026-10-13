//! `ccswap config [list|get KEY|set KEY VALUE|unset KEY|path]`, with the
//! argparse surface of upstream's dedicated `config` parser (a top-level
//! parser with `--json`/`--debug` and one subparser per verb).

use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::Error;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;
use ccswap_core::settings::{SETTING_SPECS, Settings};
use serde_json::json;

const HELP: &str = include_str!("config_help.txt");
const VERBS: [&str; 5] = ["list", "get", "set", "unset", "path"];

#[derive(Debug, Default)]
struct Args {
    verb: Option<&'static str>,
    json: bool,
    positionals: Vec<String>,
}

enum Exit {
    Help(String),
    Usage {
        usage: String,
        prog: String,
        msg: String,
    },
}

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let args = match parse(prog, argv) {
        Ok(args) => args,
        Err(Exit::Help(text)) => {
            print!("{text}");
            return 0;
        }
        Err(Exit::Usage { usage, prog, msg }) => {
            eprintln!("usage: {usage}");
            eprintln!("{prog}: error: {msg}");
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
    match execute(&args, &Settings::new(&paths), &style) {
        Ok(()) => 0,
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    pyjson::to_string_pretty(&presentation::error_envelope(&error))
                );
            } else {
                eprintln!("{}", style.red(&format!("Error: {error}")));
            }
            1
        }
    }
}

fn execute(args: &Args, settings: &Settings, style: &Style) -> Result<(), Error> {
    let positional = |i: usize| args.positionals[i].as_str();
    match args.verb.unwrap_or("list") {
        "path" => println!("{}", settings.path().display()),
        "list" => {
            let rows = settings.effective();
            if args.json {
                let entries: Vec<_> = rows
                    .iter()
                    .map(|r| json!({"key": r.spec.dotted(), "value": r.value.to_json(), "isSet": r.is_set}))
                    .collect();
                let payload = json!({
                    "schemaVersion": 1,
                    "path": settings.path().display().to_string(),
                    "settings": entries,
                });
                println!("{}", pyjson::to_string_pretty(&payload));
            } else {
                let key_w = rows
                    .iter()
                    .map(|r| r.spec.dotted().chars().count())
                    .max()
                    .unwrap_or(0);
                let val_w = rows
                    .iter()
                    .map(|r| r.value.display().chars().count())
                    .max()
                    .unwrap_or(0);
                for row in rows {
                    let line = format!(
                        "{:<key_w$}  {:<val_w$}",
                        row.spec.dotted(),
                        row.value.display()
                    );
                    if row.is_set {
                        println!("{line}");
                    } else {
                        println!("{line}  {}", style.dimmed("(default)"));
                    }
                }
            }
        }
        "get" => {
            let row = settings.get(positional(0))?;
            if args.json {
                let payload = json!({
                    "schemaVersion": 1,
                    "key": row.spec.dotted(),
                    "value": row.value.to_json(),
                    "isSet": row.is_set,
                });
                println!("{}", pyjson::to_string_pretty(&payload));
            } else {
                println!("{}", row.value.display());
            }
        }
        "set" => {
            let value = settings.set(positional(0), positional(1))?;
            println!("{} = {}", positional(0), value.display());
        }
        "unset" => {
            let key = positional(0);
            if settings.unset(key)? {
                let default = ccswap_core::settings::setting_spec(key)?.default_value();
                println!("{key} unset (default: {})", default.display());
            } else {
                eprintln!(
                    "{}",
                    style.muted(&format!("{key} is not set; nothing to do"))
                );
            }
        }
        _ => unreachable!("verbs are validated by the parser"),
    }
    Ok(())
}

fn top_usage(prog: &str) -> String {
    format!("{prog} config [-h] [--json] [--debug] {{list,get,set,unset,path}} ...")
}

fn top_help(prog: &str) -> String {
    let keys: Vec<String> = SETTING_SPECS
        .iter()
        .map(|spec| {
            format!(
                "  {:<34}{} (default {})",
                spec.dotted(),
                spec.help,
                spec.default_value().display()
            )
        })
        .collect();
    HELP.replace("{prog}", prog)
        .replace("{keys}", &keys.join("\n"))
}

struct Sub {
    json: bool,
    positionals: &'static [&'static str],
}

fn sub_spec(verb: &str) -> Sub {
    let (json, positionals): (bool, &'static [&'static str]) = match verb {
        "list" => (true, &[]),
        "get" => (true, &["KEY"]),
        "set" => (false, &["KEY", "VALUE"]),
        "unset" => (false, &["KEY"]),
        _ => (false, &[]),
    };
    Sub { json, positionals }
}

fn sub_usage(prog: &str, verb: &str, sub: &Sub) -> String {
    let mut usage = format!("{prog} config {verb} [-h]");
    if sub.json {
        usage.push_str(" [--json]");
    }
    for p in sub.positionals {
        usage.push(' ');
        usage.push_str(p);
    }
    usage
}

fn sub_help(prog: &str, verb: &str, sub: &Sub) -> String {
    let mut text = format!("usage: {}\n\n", sub_usage(prog, verb, sub));
    if !sub.positionals.is_empty() {
        text.push_str("positional arguments:\n");
        for p in sub.positionals {
            if verb == "get" {
                text.push_str("  KEY         Dotted key, e.g. autoswitch.threshold\n");
            } else {
                text.push_str(&format!("  {p}\n"));
            }
        }
        text.push('\n');
    }
    text.push_str("options:\n  -h, --help  show this help message and exit\n");
    if sub.json {
        text.push_str("  --json      Emit machine-readable JSON to stdout\n");
    }
    text
}

fn parse(prog: &str, argv: &[String]) -> Result<Args, Exit> {
    let top_error = |msg: String| Exit::Usage {
        usage: top_usage(prog),
        prog: format!("{prog} config"),
        msg,
    };
    let mut args = Args::default();
    let mut extras: Vec<String> = Vec::new();
    let mut sub_extras: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        let top_opts: &[&str] = &["-h", "--help", "--json", "--debug"];
        if !options_done && looks_like_option(arg, top_opts) {
            let (name, explicit) = split_explicit(arg);
            match lookup(name, top_opts) {
                Some(opt @ ("--json" | "--debug")) => {
                    if let Some(v) = explicit {
                        return Err(top_error(format!(
                            "argument {opt}: ignored explicit argument '{v}'"
                        )));
                    }
                    args.json |= opt == "--json";
                }
                Some(_) => return Err(Exit::Help(top_help(prog))),
                None => extras.push(arg.clone()),
            }
            continue;
        }
        let Some(verb) = VERBS.iter().find(|v| *v == arg) else {
            return Err(top_error(format!(
                "argument {{list,get,set,unset,path}}: invalid choice: '{arg}' (choose from 'list', 'get', 'set', 'unset', 'path')"
            )));
        };
        args.verb = Some(verb);
        let sub = sub_spec(verb);
        parse_sub(prog, verb, &sub, &argv[i..], &mut args, &mut sub_extras)?;
        break;
    }
    extras.extend(sub_extras);
    if !extras.is_empty() {
        return Err(top_error(format!(
            "unrecognized arguments: {}",
            extras.join(" ")
        )));
    }
    if args.json && !matches!(args.verb, None | Some("list" | "get")) {
        return Err(top_error("--json can only be used with list or get".into()));
    }
    Ok(args)
}

fn parse_sub(
    prog: &str,
    verb: &str,
    sub: &Sub,
    argv: &[String],
    args: &mut Args,
    extras: &mut Vec<String>,
) -> Result<(), Exit> {
    let opts: &[&str] = if sub.json {
        &["-h", "--help", "--json"]
    } else {
        &["-h", "--help"]
    };
    let mut options_done = false;
    for arg in argv {
        let wanted = args.positionals.len() < sub.positionals.len();
        if !options_done && arg == "--" {
            options_done = true;
            if !wanted {
                extras.push(arg.clone());
            }
            continue;
        }
        if !options_done && looks_like_option(arg, opts) {
            let (name, explicit) = split_explicit(arg);
            match lookup(name, opts) {
                Some("--json") => {
                    if let Some(v) = explicit {
                        return Err(Exit::Usage {
                            usage: sub_usage(prog, verb, sub),
                            prog: format!("{prog} config {verb}"),
                            msg: format!("argument --json: ignored explicit argument '{v}'"),
                        });
                    }
                    args.json = true;
                }
                Some(_) => return Err(Exit::Help(sub_help(prog, verb, sub))),
                None => extras.push(arg.clone()),
            }
        } else if wanted {
            args.positionals.push(arg.clone());
        } else {
            extras.push(arg.clone());
        }
    }
    let missing = &sub.positionals[args.positionals.len()..];
    if !missing.is_empty() {
        return Err(Exit::Usage {
            usage: sub_usage(prog, verb, sub),
            prog: format!("{prog} config {verb}"),
            msg: format!(
                "the following arguments are required: {}",
                missing.join(", ")
            ),
        });
    }
    Ok(())
}

pub(crate) fn split_explicit(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((name, value)) if arg.starts_with("--") => (name, Some(value)),
        _ => (arg, None),
    }
}

pub(crate) fn lookup(name: &str, opts: &[&'static str]) -> Option<&'static str> {
    if let Some(opt) = opts.iter().find(|o| **o == name) {
        return Some(opt);
    }
    if !name.starts_with("--") || name.len() < 3 {
        return None;
    }
    let mut matches = opts.iter().filter(|o| o.starts_with(name));
    match (matches.next(), matches.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// argparse's classification: known options first, then `-`, negative
/// numbers and anything containing a space are positionals.
pub(crate) fn looks_like_option(arg: &str, opts: &[&'static str]) -> bool {
    if !arg.starts_with('-') || arg == "-" {
        return false;
    }
    if lookup(split_explicit(arg).0, opts).is_some() {
        return true;
    }
    !(is_negative_number(arg) || arg.contains(' '))
}

fn is_negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    match rest.split_once('.') {
        None => all_digits(rest),
        Some((int, frac)) => (int.is_empty() || all_digits(int)) && all_digits(frac),
    }
}
