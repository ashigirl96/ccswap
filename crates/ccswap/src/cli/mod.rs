pub mod argparse;
pub mod argv;
pub mod config;
pub mod namespace;
pub mod purge;

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use ccswap_claude::accounts;
use ccswap_claude::actions::LiveActions;
use ccswap_claude::migrations;
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::console::Console;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;
use ccswap_core::settings::Settings;
use ccswap_core::update_check;

use crate::ui::launch_agent::{self, LaunchAgent};
use argparse::{Action, Opt, Outcome, ParseError, Parsed, Parser};

const HELP: &str = include_str!("help.txt");

const OPTS: &[Opt] = &[
    Opt::flag("-h").action(Action::Help),
    Opt::flag("--help").action(Action::Help),
    Opt::flag("--version").action(Action::Version),
    Opt::flag("--debug"),
    Opt::flag("--token-status"),
    Opt::flag("--json"),
    Opt::value("--strategy").choices(&["best", "next-available"]),
    Opt::value("--model"),
    Opt::value("--slot").int(),
    Opt::value("--email"),
    Opt::value("--account"),
    Opt::value("--alias"),
    Opt::flag("--force"),
    Opt::flag("--full"),
    Opt::flag("--install-service"),
    Opt::flag("--uninstall-service"),
    Opt::flag("--service-status"),
    Opt::flag("--add-account").exclusive(),
    Opt::value("--remove-account").exclusive(),
    Opt::value("--disable-account").exclusive(),
    Opt::value("--enable-account").exclusive(),
    Opt::flag("--list").exclusive(),
    Opt::flag("--switch").exclusive(),
    Opt::value("--switch-to").exclusive(),
    Opt::flag("--status").exclusive(),
    Opt::flag("--purge").exclusive(),
    Opt::value("--export").exclusive(),
    Opt::value("--import").exclusive(),
    Opt::flag("--tui").exclusive(),
    Opt::flag("--watch").exclusive(),
    Opt::flag("--menubar").exclusive(),
    Opt::flag("--upgrade").exclusive(),
    Opt::optional_value("--add-token").exclusive(),
];

const REMOVE_OPTS: &[Opt] = &[Opt::flag("-y"), Opt::flag("--yes")];

const COMMAND_FLAGS: &[&str] = &[
    "--add-account",
    "--remove-account",
    "--disable-account",
    "--enable-account",
    "--list",
    "--switch",
    "--switch-to",
    "--status",
    "--purge",
    "--export",
    "--import",
    "--tui",
    "--watch",
    "--menubar",
    "--upgrade",
    "--add-token",
];

/// The app verbs the global parser reads; Claude Code's are in [`crate::claude::VERBS`].
const SUBCOMMAND_FLAGS: &[(&str, &str)] = &[
    ("help", "--help"),
    ("purge", "--purge"),
    ("upgrade", "--upgrade"),
    ("update", "--upgrade"),
    ("tui", "--tui"),
    ("watch", "--watch"),
    ("menubar", "--menubar"),
];

/// The verbs the global parser reads, with the help their `--help` prints.
pub type VerbHelp = &'static [(&'static [&'static str], &'static str)];

const VERB_HELP: VerbHelp = &[
    (&["purge"], include_str!("help/purge.txt")),
    (&["upgrade", "update"], include_str!("help/upgrade.txt")),
];

/// The help of a verb given `-h`/`--help`. Checked before the global parse,
/// which would read `remove --help` as `--remove-account` missing its value.
fn verb_help(args: &[String]) -> Option<&'static str> {
    let (verb, rest) = args.split_first()?;
    let claude = crate::claude::VERBS
        .iter()
        .filter_map(|v| Some((v.names, v.help?)));
    let (_, help) = claude
        .chain(VERB_HELP.iter().chain(crate::ui::VERB_HELP).copied())
        .find(|(names, _)| names.contains(&verb.as_str()))?;
    rest.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "-h" || arg == "--help")
        .then_some(help)
}

pub fn prog_name(argv0: Option<&str>) -> String {
    let name = argv0
        .and_then(|a| std::path::Path::new(a).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.is_empty() {
        "ccswap".into()
    } else {
        name
    }
}

pub struct Cli {
    prog: String,
    env: Environment,
    stdout: Vec<String>,
}

impl Cli {
    pub fn new(prog: String, env: Environment) -> Self {
        Cli {
            prog,
            env,
            stdout: Vec::new(),
        }
    }

    pub fn run(&mut self, args: Vec<String>) -> i32 {
        for warning in
            Settings::new(&Paths::new(&self.env), ccswap_claude::SETTINGS).ui_load_warnings()
        {
            eprintln!("{warning}");
        }
        let args = match args.split_first() {
            Some((namespace, rest)) if namespace == "claude" => {
                match crate::claude::resolve(&self.prog, rest) {
                    Ok(bare) => bare,
                    Err(code) => return code,
                }
            }
            _ => args,
        };
        presentation::probe_cli_background(&self.env, &Paths::new(&self.env), &args);
        if args.first().is_some_and(|v| v == "codex") {
            return crate::codex::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "config") {
            return config::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "run") {
            return crate::claude::run::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "alias") {
            return crate::claude::manage::run_alias(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "auto") {
            return crate::claude::auto::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "map") {
            return crate::claude::map::run_map(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "unmap") {
            return crate::claude::map::run_unmap(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "widget") {
            return crate::ui::widget::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "unclaimed") {
            return crate::claude::unclaimed::run(&self.prog, &self.env, &args[1..]);
        }
        if let Some(verb @ ("swap" | "move")) = args.first().map(String::as_str) {
            return crate::claude::renumber::run(&self.prog, &self.env, verb, &args[1..]);
        }
        let mut args = args;
        let opened_as_app = args.is_empty() && launched_as_app(&self.env);
        if opened_as_app {
            args = vec!["menubar".into()];
        } else if args.is_empty()
            && std::io::stdout().is_terminal()
            && std::io::stdin().is_terminal()
        {
            args = vec!["--tui".into()];
        }
        if let Some(help) = verb_help(&args) {
            print!("{}", help.replace("{prog}", &self.prog));
            return 0;
        }
        let args = translate_subcommand(args);

        let opts: Vec<Opt> = if args.first().is_some_and(|a| a == "--remove-account") {
            OPTS.iter().chain(REMOVE_OPTS).copied().collect()
        } else {
            OPTS.to_vec()
        };
        let parsed = match (Parser { opts: &opts }).parse(&args) {
            Ok((Outcome::Help, _)) => {
                print!("{}", HELP.replace("{prog}", &self.prog));
                return 0;
            }
            Ok((Outcome::Version, _)) => {
                println!("{} {}", launch_agent::PROGRAM_NAME, ccswap_core::VERSION);
                return 0;
            }
            Ok((Outcome::Parsed, parsed)) => parsed,
            Err(ParseError(msg)) => return self.usage_error(&msg),
        };
        if let Err(msg) = self.validate(&parsed) {
            return self.usage_error(&msg);
        }

        // Before any store access, so a broken store cannot block it.
        if parsed.flag("--upgrade") {
            println!("{}", update_check::UPGRADE_INSTRUCTIONS);
            return 0;
        }

        let paths = Paths::new(&self.env);
        let style = Style::detect(&self.env, &paths);
        let json = parsed.flag("--json");
        if is_root() {
            eprintln!(
                "{}",
                style.red("Error: Do not run this script as root (unless running in a container)")
            );
            return 1;
        }
        run_store_migrations(&self.env, &paths, parsed.flag("--debug"));
        for (flag, start) in [
            ("--tui", crate::ui::tui::Start::Dashboard),
            ("--watch", crate::ui::tui::Start::Watch),
        ] {
            if parsed.flag(flag) {
                let actions = Arc::new(LiveActions::new(self.env.clone()));
                return crate::ui::tui::run(actions, &self.env, start);
            }
        }
        if parsed.flag("--menubar") {
            if !cfg!(target_os = "macos") {
                eprintln!(
                    "{}",
                    style.red("Error: The menu bar is only available on macOS.")
                );
                return 1;
            }
            if parsed.flag("--install-service")
                || parsed.flag("--uninstall-service")
                || parsed.flag("--service-status")
            {
                return match crate::ui::service::run(&self.prog, &self.env, &parsed, &style) {
                    Ok(()) => 0,
                    Err(error) => {
                        if json {
                            println!(
                                "{}",
                                pyjson::to_string_pretty(&presentation::error_envelope(&error))
                            );
                        } else {
                            eprintln!("{}", style.red(&format!("Error: {error}")));
                        }
                        1
                    }
                };
            }
            // A second in-process menu bar would add a duplicate status item.
            if opened_as_app && LaunchAgent::new(&self.env).kickstart() {
                return 0;
            }
            let actions = Arc::new(LiveActions::new(self.env.clone()));
            return crate::ui::menubar::run(actions, self.env.clone());
        }
        let result = if parsed.flag("--purge") {
            purge::run(&self.env, &paths, &style, &mut TerminalConsole)
        } else {
            crate::claude::accounts::dispatch(
                &self.prog,
                &self.env,
                &paths,
                &style,
                &parsed,
                &mut self.stdout,
            )
            .unwrap_or(Ok(()))
        };
        match result {
            Ok(()) => {
                for line in self.stdout.drain(..) {
                    println!("{line}");
                }
                // After purge the check would recreate the store it just removed.
                if !json && !parsed.flag("--purge") {
                    let endpoints = Endpoints::new(&self.env);
                    if let Some(notice) =
                        update_check::check_for_update(&paths, &endpoints, ccswap_core::VERSION)
                    {
                        eprintln!("\n{}", style.muted(&notice));
                    }
                }
                0
            }
            Err(error) => {
                if json {
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

    /// Flag-combination checks, in upstream's order.
    fn validate(&self, p: &Parsed) -> Result<(), String> {
        let has = |name: &str| p.flag(name) || p.value(name).is_some();
        let any_command = COMMAND_FLAGS.iter().any(|f| has(f));
        if !any_command {
            return Err(format!("no command given — try '{} help'", self.prog));
        }
        let checks: [(bool, &str); 12] = [
            (
                p.flag("--token-status") && !p.flag("--list"),
                "--token-status can only be used with 'list'",
            ),
            (
                p.flag("--json")
                    && !(p.flag("--list")
                        || p.flag("--status")
                        || p.flag("--switch")
                        || nonempty(p, "--switch-to")),
                "--json can only be used with 'list', 'status', or 'switch'",
            ),
            (
                p.flag("--json") && p.flag("--token-status"),
                "--token-status cannot be combined with --json",
            ),
            (
                has("--strategy") && !p.flag("--switch"),
                "--strategy can only be used with bare 'switch'",
            ),
            (
                has("--model") && !has("--strategy"),
                "--model can only be used with 'switch --strategy best' or 'switch --strategy next-available'",
            ),
            (
                has("--slot") && !(p.flag("--add-account") || has("--add-token")),
                "--slot can only be used with 'add' or 'add-token'",
            ),
            (
                has("--email") && !has("--add-token"),
                "--email can only be used with 'add-token'",
            ),
            (
                has("--account") && !nonempty(p, "--export"),
                "--account can only be used with 'export'",
            ),
            (
                has("--alias") && !p.flag("--add-account"),
                "--alias can only be used with 'add'",
            ),
            (
                p.flag("--force") && !(nonempty(p, "--import") || nonempty(p, "--switch-to")),
                "--force can only be used with 'import' or 'switch <num|email>'",
            ),
            (
                p.flag("--full") && !nonempty(p, "--export"),
                "--full can only be used with 'export'",
            ),
            (
                (p.flag("--install-service")
                    || p.flag("--uninstall-service")
                    || p.flag("--service-status"))
                    && !p.flag("--menubar"),
                "--install-service, --uninstall-service and --service-status can only be used with 'menubar'",
            ),
        ];
        match checks.iter().find(|(failed, _)| *failed) {
            Some((_, msg)) => Err((*msg).to_string()),
            None => Ok(()),
        }
    }

    fn usage_error(&self, msg: &str) -> i32 {
        eprintln!("usage: {} <command> [args] [options]", self.prog);
        eprintln!("{}: error: {msg}", self.prog);
        2
    }
}

/// Runs after the root check, unlike upstream, so root never writes into the
/// user's store.
pub fn run_store_migrations(env: &Environment, paths: &Paths, debug: bool) {
    ccswap_core::log::setup(&paths.backup_root(), debug);
    let keychain = Keychain::new(Endpoints::new(env).security, env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    for line in migrations::run_migrations(paths, &secrets).stderr {
        eprintln!("{line}");
    }
}

/// Prints as it goes, so output already shown survives a later error the
/// way Python's `print` does.
pub(crate) struct TerminalConsole;

impl Console for TerminalConsole {
    fn line(&mut self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    fn prompt(&mut self, question: &str) -> Option<String> {
        {
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "{question}");
            let _ = out.flush();
        }
        let mut answer = String::new();
        match std::io::stdin().read_line(&mut answer) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(answer.trim_end_matches(['\n', '\r']).to_string()),
        }
    }

    fn secret(&mut self, question: &str) -> Option<String> {
        accounts::getpass(question)
    }
}

pub(crate) fn nonempty(p: &Parsed, name: &str) -> bool {
    p.value(name).is_some_and(|v| !v.is_empty())
}

/// LaunchServices sets this for the app it opens; a shell inherits its
/// terminal's id instead.
fn launched_as_app(env: &Environment) -> bool {
    env.get("__CFBundleIdentifier")
        .is_some_and(|id| id == launch_agent::BUNDLE_ID)
}

fn translate_subcommand(args: Vec<String>) -> Vec<String> {
    let Some((verb, rest)) = args.split_first() else {
        return args;
    };
    if verb == "switch" {
        let flag = match rest.first() {
            Some(next) if !next.starts_with('-') => "--switch-to",
            _ => "--switch",
        };
        return std::iter::once(flag.to_string())
            .chain(rest.iter().cloned())
            .collect();
    }
    let flag = match SUBCOMMAND_FLAGS.iter().find(|(v, _)| v == verb) {
        Some((_, flag)) => Some(*flag),
        None => crate::claude::find(verb).and_then(|v| v.flag),
    };
    match flag {
        Some(flag) => std::iter::once(flag.to_string())
            .chain(rest.iter().cloned())
            .collect(),
        None => args,
    }
}
