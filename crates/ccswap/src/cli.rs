use std::io::{IsTerminal, Write};

use ccswap_core::accounts::{self, Accounts, AddOptions, AddTokenOptions, Console, TokenInput};
use ccswap_core::anthropic_client::AnthropicClient;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::error::{Error, ErrorKind};
use ccswap_core::migrations;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{self, Style};
use ccswap_core::pyjson;
use ccswap_core::secret_store::{Keychain, SecretStore};
use ccswap_core::settings::Settings;
use ccswap_core::status;

use crate::argparse::{Action, Opt, Outcome, ParseError, Parsed, Parser};

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

/// Commands parsed by their own parser before the main one.
const PRE_DISPATCHED: &[&str] = &[
    "run",
    "auto",
    "config",
    "map",
    "unmap",
    "unclaimed",
    "alias",
    "swap",
    "move",
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

    /// Runs one invocation and returns the exit code.
    pub fn run(&mut self, args: Vec<String>) -> i32 {
        for warning in Settings::new(&Paths::new(&self.env)).ui_load_warnings() {
            eprintln!("{warning}");
        }
        if args.first().is_some_and(|v| v == "config") {
            return crate::config_cmd::run(&self.prog, &self.env, &args[1..]);
        }
        if let Some(verb) = args
            .first()
            .filter(|v| PRE_DISPATCHED.contains(&v.as_str()))
        {
            return self.not_implemented(verb);
        }
        let mut args = args;
        if args.is_empty() && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            args = vec!["--tui".into()];
        }
        let args = translate_subcommand(args);

        let parsed = match (Parser { opts: OPTS }).parse(&args) {
            Ok((Outcome::Help, _)) => {
                print!("{}", HELP.replace("{prog}", &self.prog));
                return 0;
            }
            Ok((Outcome::Version, _)) => {
                println!("{} {}", self.prog, ccswap_core::VERSION);
                return 0;
            }
            Ok((Outcome::Parsed, parsed)) => parsed,
            Err(ParseError(msg)) => return self.usage_error(&msg),
        };
        if let Err(msg) = self.validate(&parsed) {
            return self.usage_error(&msg);
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
        if !parsed.flag("--upgrade") {
            run_store_migrations(&self.env, &paths);
        }
        let result = if parsed.flag("--status") {
            self.status(&paths, &style, json)
        } else if parsed.flag("--add-account") {
            self.add(&paths, &style, &parsed)
        } else if let Some(token) = parsed.value("--add-token") {
            self.add_token(&paths, &style, token, &parsed)
        } else {
            let command = COMMAND_FLAGS
                .iter()
                .find(|f| parsed.flag(f) || parsed.value(f).is_some())
                .copied()
                .unwrap_or_default();
            Err(not_implemented_error(command.trim_start_matches('-')))
        };
        match result {
            Ok(()) => {
                for line in self.stdout.drain(..) {
                    println!("{line}");
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

    fn status(&mut self, paths: &Paths, style: &Style, json: bool) -> Result<(), Error> {
        let endpoints = Endpoints::new(&self.env);
        let keychain = Keychain::new(endpoints.security.clone(), &self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), &self.env);
        let report = status::status(paths, &secrets)?;
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

    fn not_implemented(&self, verb: &str) -> i32 {
        eprintln!("Error: {}", not_implemented_error(verb));
        1
    }
}

/// Upstream runs these wherever it builds its switcher: every command but
/// `upgrade`, before the root check (ccswap runs them after it so root never
/// writes into the user's store).
pub fn run_store_migrations(env: &Environment, paths: &Paths) {
    let keychain = Keychain::new(Endpoints::new(env).security, env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    for line in migrations::run_migrations(paths, &secrets).stderr {
        eprintln!("{line}");
    }
}

/// Prints as it goes, so output already shown survives a later error the
/// way Python's `print` does.
struct TerminalConsole;

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

fn not_implemented_error(command: &str) -> Error {
    Error::new(
        ErrorKind::ClaudeSwitchError,
        format!("'{command}' is not implemented yet in ccswap"),
    )
}

/// `ccswap list` → `--list`, `ccswap switch X` → `--switch-to X`, etc.
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
