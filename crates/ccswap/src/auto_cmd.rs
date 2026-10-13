//! The pre-dispatched `auto`: the auto-switch engine, one tick with
//! `--once` (exit code = outcome), as JSONL with `--json`.

use std::io::Write;

use ccswap_core::anthropic_client::AnthropicClient;
use ccswap_core::autoswitch::{
    AutoStrategy, AutoSwitchEngine, AutoSwitchEvent, AutoSwitchSettings, CliOverrides, SwitcherHost,
};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use ccswap_core::pyjson;
use ccswap_core::secret_store::{Keychain, SecretStore};
use ccswap_core::settings::python_float;

use crate::argparse::{Opt, Outcome, ParseError, Parsed, Parser};

const HELP: &str = include_str!("auto_help.txt");

const OPTS: &[Opt] = &[
    Opt::flag("-h").action(crate::argparse::Action::Help),
    Opt::flag("--help").action(crate::argparse::Action::Help),
    Opt::flag("--once"),
    Opt::flag("--json"),
    Opt::value("--interval").float(),
    Opt::value("--threshold").float(),
    Opt::value("--cooldown").float(),
    Opt::value("--model"),
    Opt::flag("--include-api-key-accounts"),
    Opt::flag("--no-include-api-key-accounts"),
    Opt::value("--strategy").choices(&["best", "consume-first"]),
    Opt::flag("--dry-run"),
    Opt::flag("--debug"),
];

fn usage(prog: &str) -> String {
    HELP.lines()
        .take(4)
        .collect::<Vec<_>>()
        .join("\n")
        .replace("{prog}", prog)
}

fn overrides(parsed: &Parsed) -> CliOverrides {
    let float = |name: &str| parsed.value(name).and_then(python_float);
    CliOverrides {
        threshold: float("--threshold"),
        interval: float("--interval"),
        cooldown: float("--cooldown"),
        include_api_key_accounts: parsed
            .last_of(&[
                "--include-api-key-accounts",
                "--no-include-api-key-accounts",
            ])
            .map(|flag| flag == "--include-api-key-accounts"),
        model: parsed.value("--model").map(str::to_string),
        strategy: parsed.value("--strategy").and_then(AutoStrategy::parse),
    }
}

fn print_flushed(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn human_line(style: &Style, event: &AutoSwitchEvent) -> String {
    let line = event.human();
    let line = match event.name() {
        "switch" => style.accent(&line),
        "error" | "account-quarantined" => style.yellowed(&line),
        "poll" | "no-switch" | "sleep" => style.dimmed(&line),
        _ => line,
    };
    format!("{}  {line}", chrono::Local::now().format("%H:%M:%S"))
}

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let parsed = match (Parser { opts: OPTS }).parse(argv) {
        Ok((Outcome::Help, _)) => {
            print!("{}", HELP.replace("{prog}", prog));
            return 0;
        }
        Ok((_, parsed)) => parsed,
        Err(ParseError(msg)) => {
            eprintln!("{}", usage(prog));
            eprintln!("{prog} auto: error: {msg}");
            return 2;
        }
    };
    let json = parsed.flag("--json");

    let paths = Paths::new(env);
    let style = Style::detect(env, &paths);
    if is_root() {
        eprintln!(
            "{}",
            style.red("Error: Do not run this script as root (unless running in a container)")
        );
        return 1;
    }
    crate::cli::run_store_migrations(env, &paths, parsed.flag("--debug"));
    let settings = AutoSwitchSettings::load(&paths).merged_with_cli(&overrides(&parsed));

    if !parsed.flag("--once") {
        eprintln!(
            "{}",
            style.red("Error: 'auto' without --once is not implemented yet in ccswap")
        );
        return 1;
    }

    let endpoints = Endpoints::new(env);
    let keychain = Keychain::new(endpoints.security.clone(), env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    let client = AnthropicClient::new(endpoints);
    let host = SwitcherHost::new(&paths, env, &secrets, &client, &style);
    let sink_style = &style;
    let mut engine = AutoSwitchEngine::new(
        &host,
        settings,
        Box::new(move |event: AutoSwitchEvent| {
            if json {
                print_flushed(&pyjson::to_string_compact(&event.to_json()));
            } else {
                print_flushed(&human_line(sink_style, &event));
            }
        }),
    )
    .dry_run(parsed.flag("--dry-run"));
    engine.tick().exit_code()
}
