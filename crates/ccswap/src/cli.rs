use std::io::{IsTerminal, Write};
use std::sync::Arc;

use ccswap_core::accounts::{self, Accounts, AddOptions, AddTokenOptions, Console, TokenInput};
use ccswap_core::actions::LiveActions;
use ccswap_core::anthropic_client::AnthropicClient;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{Error, ErrorKind};
use ccswap_core::listing::{self, ListOptions, ListReport};
use ccswap_core::migrations;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;
use ccswap_core::ranking::Strategy;
use ccswap_core::secret_store::{Keychain, SecretStore};
use ccswap_core::settings::{Settings, parse_model_names};
use ccswap_core::status;
use ccswap_core::switcher::{ModelSource, SwitchStrategy, Switcher};
use ccswap_core::transfer::Transfer;
use ccswap_core::update_check;
use ccswap_core::usage_view;

use crate::argparse::{Action, Opt, Outcome, ParseError, Parsed, Parser};
use crate::launch_agent::{self, LaunchAgent};

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

const SUBCOMMAND_FLAGS: &[(&str, &str)] = &[
    ("help", "--help"),
    ("list", "--list"),
    ("ls", "--list"),
    ("status", "--status"),
    ("add", "--add-account"),
    ("add-token", "--add-token"),
    ("remove", "--remove-account"),
    ("rm", "--remove-account"),
    ("disable", "--disable-account"),
    ("enable", "--enable-account"),
    ("export", "--export"),
    ("import", "--import"),
    ("purge", "--purge"),
    ("upgrade", "--upgrade"),
    ("update", "--upgrade"),
    ("tui", "--tui"),
    ("watch", "--watch"),
    ("menubar", "--menubar"),
];

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
        for warning in Settings::new(&Paths::new(&self.env)).ui_load_warnings() {
            eprintln!("{warning}");
        }
        presentation::probe_cli_background(&self.env, &Paths::new(&self.env), &args);
        if args.first().is_some_and(|v| v == "config") {
            return crate::config_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "run") {
            return crate::run_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "alias") {
            return crate::manage_cmd::run_alias(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "auto") {
            return crate::auto_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "map") {
            return crate::map_cmd::run_map(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "unmap") {
            return crate::map_cmd::run_unmap(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "widget") {
            return crate::widget_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if args.first().is_some_and(|v| v == "unclaimed") {
            return crate::unclaimed_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if let Some(verb @ ("swap" | "move")) = args.first().map(String::as_str) {
            return crate::renumber_cmd::run(&self.prog, &self.env, verb, &args[1..]);
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
        let args = translate_subcommand(args);

        let parsed = match (Parser { opts: OPTS }).parse(&args) {
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
            ("--tui", crate::tui::Start::Dashboard),
            ("--watch", crate::tui::Start::Watch),
        ] {
            if parsed.flag(flag) {
                let actions = Arc::new(LiveActions::new(self.env.clone()));
                return crate::tui::run(actions, &self.env, start);
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
                return match self.menubar_service(&parsed, &style) {
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
            return crate::menubar::run(actions, self.env.clone());
        }
        let result = if parsed.flag("--status") {
            self.status(&paths, &style, json)
        } else if parsed.flag("--list") {
            self.list(&paths, &style, json, parsed.flag("--token-status"))
        } else if parsed.flag("--add-account") {
            self.add(&paths, &style, &parsed)
        } else if let Some(destination) = parsed.value("--export") {
            self.export(&paths, destination, &parsed)
        } else if let Some(source) = parsed.value("--import") {
            self.import(&paths, source, parsed.flag("--force"))
        } else if let Some(token) = parsed.value("--add-token") {
            self.add_token(&paths, &style, token, &parsed)
        } else if parsed.flag("--switch") || nonempty(&parsed, "--switch-to") {
            self.switch(&paths, &style, &parsed)
        } else if let Some(result) = crate::manage_cmd::dispatch(&self.env, &paths, &style, &parsed)
        {
            result
        } else {
            Ok(())
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

    fn menubar_service(&self, parsed: &Parsed, style: &Style) -> Result<(), Error> {
        let agent = LaunchAgent::new(&self.env);
        if parsed.flag("--install-service") {
            let program = launch_agent::resolve_program(
                std::env::args_os().next().as_deref(),
                &std::env::current_dir().unwrap_or_default(),
                ccswap_core::session::which(&self.env, launch_agent::PROGRAM_NAME),
                || std::env::current_exe().ok(),
            );
            let installed = agent.install(program)?;
            println!("Menu bar service installed ({}).", installed.label);
            println!("  plist: {}", installed.plist.display());
            println!("  logs:  {}", installed.stderr_log.display());
            println!(
                "{}",
                style.dimmed(&format!(
                    "It starts at login from now on. Re-run this after a {} upgrade to point launchd at the new build.",
                    self.prog
                ))
            );
            return Ok(());
        }
        if parsed.flag("--uninstall-service") {
            let removed = agent.uninstall()?;
            if removed.was_loaded || removed.removed_plist {
                println!("Menu bar service removed.");
            } else {
                println!("Menu bar service was not installed.");
            }
            return Ok(());
        }
        let status = agent.status()?;
        if !status.installed && !status.loaded {
            println!("Menu bar service is not installed.");
            println!(
                "{}",
                style.dimmed(&format!(
                    "Install it with: {} menubar --install-service",
                    self.prog
                ))
            );
            return Ok(());
        }
        let state = status
            .state
            .clone()
            .unwrap_or_else(|| if status.loaded { "loaded" } else { "stopped" }.to_string());
        let pid = status
            .pid
            .filter(|p| *p != 0)
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default();
        println!("Menu bar service: {state}{pid}");
        println!("  plist: {}", status.plist.display());
        if !status.installed {
            println!(
                "{}",
                style.dimmed("launchd still has it loaded, but the plist is gone.")
            );
        }
        Ok(())
    }

    fn status(&mut self, paths: &Paths, style: &Style, json: bool) -> Result<(), Error> {
        let endpoints = Endpoints::new(&self.env);
        let keychain = Keychain::new(endpoints.security.clone(), &self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), &self.env);
        let client = AnthropicClient::new(endpoints);
        let report = status::status(paths, &self.env, &secrets, &client)?;
        if json {
            self.stdout
                .push(pyjson::to_string_pretty(&presentation::status_json(
                    &report,
                )));
        } else {
            self.stdout
                .extend(presentation::status_lines(&report, style));
        }
        Ok(())
    }

    fn list(
        &mut self,
        paths: &Paths,
        style: &Style,
        json: bool,
        token_status: bool,
    ) -> Result<(), Error> {
        let endpoints = Endpoints::new(&self.env);
        let keychain = Keychain::new(endpoints.security.clone(), &self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), &self.env);
        let client = AnthropicClient::new(endpoints);
        let options = ListOptions { token_status, json };
        let report = listing::list(paths, &self.env, &secrets, &client, options)?;
        if json {
            self.stdout
                .push(pyjson::to_string_pretty(&usage_view::list_json(&report)));
            return Ok(());
        }
        if report == ListReport::NoStore {
            TerminalConsole.line(&style.dimmed("No accounts are managed yet."));
            return self.first_run_setup(paths, style, &secrets, &client);
        }
        self.stdout.extend(usage_view::list_lines(&report, style));
        Ok(())
    }

    fn first_run_setup(
        &mut self,
        paths: &Paths,
        style: &Style,
        secrets: &SecretStore,
        client: &AnthropicClient,
    ) -> Result<(), Error> {
        let mut console = TerminalConsole;
        let Some(identity) = ccswap_core::store::current_identity(paths) else {
            console.line(&style.dimmed("No active Claude account found. Please log in first."));
            return Ok(());
        };
        let answer = console
            .prompt(&format!(
                "No managed accounts found. Add current account ({}) to managed list? [Y/n] ",
                identity.email
            ))
            .ok_or_else(|| Error::new(ErrorKind::ClaudeSwitchError, "EOF when reading a line"))?;
        if answer.to_lowercase() == "n" {
            console.line(&style.dimmed(&format!(
                "Setup cancelled. You can run '{} --add-account' later.",
                self.prog
            )));
            return Ok(());
        }
        let accounts = Accounts {
            paths,
            env: &self.env,
            secrets,
            client,
            style,
        };
        accounts.add_account(&AddOptions::default(), &mut console)
    }

    fn add(&mut self, paths: &Paths, style: &Style, parsed: &Parsed) -> Result<(), Error> {
        let endpoints = Endpoints::new(&self.env);
        let keychain = Keychain::new(endpoints.security.clone(), &self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), &self.env);
        let client = AnthropicClient::new(endpoints);
        let accounts = Accounts {
            paths,
            env: &self.env,
            secrets: &secrets,
            client: &client,
            style,
        };
        let options = AddOptions {
            slot: parsed.value("--slot").and_then(|s| s.trim().parse().ok()),
            alias: parsed.value("--alias").map(str::to_string),
            assume_yes: false,
        };
        accounts.add_account(&options, &mut TerminalConsole)
    }

    fn secrets(&self, paths: &Paths) -> SecretStore {
        let keychain = Keychain::new(Endpoints::new(&self.env).security, &self.env);
        SecretStore::new(keychain, paths.clone(), &self.env)
    }

    /// An empty destination selects nothing, as upstream's truthiness dispatch does.
    fn export(&mut self, paths: &Paths, destination: &str, parsed: &Parsed) -> Result<(), Error> {
        if destination.is_empty() {
            return Ok(());
        }
        let secrets = self.secrets(paths);
        let transfer = Transfer {
            paths,
            secrets: &secrets,
        };
        if let Some(envelope) = transfer.export(
            destination,
            parsed.value("--account"),
            parsed.flag("--full"),
            &mut |line| eprintln!("{line}"),
        )? {
            self.stdout.push(envelope);
        }
        Ok(())
    }

    fn switch(&mut self, paths: &Paths, style: &Style, parsed: &Parsed) -> Result<(), Error> {
        let strategy = switch_strategy(paths, parsed);
        let secrets = self.secrets(paths);
        let client = AnthropicClient::new(Endpoints::new(&self.env));
        let env = &self.env;
        let show_list = |console: &mut dyn Console| -> Result<(), Error> {
            let list_secrets = self.secrets(paths);
            let report = listing::list(paths, env, &list_secrets, &client, ListOptions::default())?;
            for line in usage_view::list_lines(&report, style) {
                console.line(&line);
            }
            Ok(())
        };
        let switcher = Switcher {
            paths,
            env: &self.env,
            secrets: &secrets,
            client: &client,
            style,
            listing: Some(&show_list),
        };
        let json = parsed.flag("--json");
        let report = match parsed.value("--switch-to") {
            Some(target) => {
                switcher.switch_to(target, json, parsed.flag("--force"), &mut TerminalConsole)?
            }
            None => switcher.switch_next(strategy.as_ref(), json, &mut TerminalConsole)?,
        };
        if let Some(report) = report.filter(|_| json) {
            let mut payload = report.to_json();
            if let Some(s) = strategy.as_ref().filter(|s| !s.models.is_empty()) {
                payload["models"] = s.models.clone().into();
                payload["modelSource"] = s.model_source.map(ModelSource::as_str).into();
            }
            self.stdout.push(pyjson::to_string_pretty(&payload));
        }
        Ok(())
    }

    fn import(&mut self, paths: &Paths, source: &str, force: bool) -> Result<(), Error> {
        if source.is_empty() {
            return Ok(());
        }
        let secrets = self.secrets(paths);
        let transfer = Transfer {
            paths,
            secrets: &secrets,
        };
        transfer.import(
            source,
            force,
            &mut || std::io::read_to_string(std::io::stdin()),
            &mut |line| eprintln!("{line}"),
        )
    }

    fn add_token(
        &mut self,
        paths: &Paths,
        style: &Style,
        token: &str,
        parsed: &Parsed,
    ) -> Result<(), Error> {
        let endpoints = Endpoints::new(&self.env);
        let keychain = Keychain::new(endpoints.security.clone(), &self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), &self.env);
        let client = AnthropicClient::new(endpoints);
        let accounts = Accounts {
            paths,
            env: &self.env,
            secrets: &secrets,
            client: &client,
            style,
        };
        let input = match token {
            "-" => TokenInput::Stdin,
            "" => TokenInput::Prompt,
            value => TokenInput::Value(value.to_string()),
        };
        let options = AddTokenOptions {
            email: parsed.value("--email").map(str::to_string),
            slot: parsed.value("--slot").and_then(|s| s.trim().parse().ok()),
            assume_yes: false,
        };
        accounts.add_token(input, &options, &mut TerminalConsole)
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

fn nonempty(p: &Parsed, name: &str) -> bool {
    p.value(name).is_some_and(|v| !v.is_empty())
}

fn switch_strategy(paths: &Paths, parsed: &Parsed) -> Option<SwitchStrategy> {
    let mut chosen = SwitchStrategy::new(Strategy::parse(parsed.value("--strategy")?)?);
    if let Some(names) = parsed.value("--model") {
        chosen.models = parse_model_names(names);
        chosen.model_source = Some(ModelSource::Cli);
    } else {
        chosen.models = Settings::new(paths).autoswitch_models();
        chosen.model_source = (!chosen.models.is_empty()).then_some(ModelSource::Setting);
    }
    Some(chosen)
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
    match SUBCOMMAND_FLAGS.iter().find(|(v, _)| v == verb) {
        Some((_, flag)) => std::iter::once(flag.to_string())
            .chain(rest.iter().cloned())
            .collect(),
        None => args,
    }
}
