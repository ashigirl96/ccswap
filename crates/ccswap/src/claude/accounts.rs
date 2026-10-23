use ccswap_claude::accounts::{Accounts, AddOptions, AddTokenOptions, TokenInput};
use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::autoswitch::{autoswitch_models, parse_model_names};
use ccswap_claude::listing::{self, ListOptions, ListReport};
use ccswap_claude::ranking::Strategy;
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_claude::status;
use ccswap_claude::switcher::{ModelSource, SwitchStrategy, Switcher};
use ccswap_claude::transfer::Transfer;
use ccswap_core::console::Console;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::Environment;
use ccswap_core::error::{Error, ErrorKind};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use ccswap_core::pyjson;
use ccswap_core::settings::Settings;

use crate::cli::argparse::Parsed;
use crate::cli::{TerminalConsole, nonempty};

/// Runs the Claude Code command the global parser selected; `None` when it
/// selected none. Lines pushed to `stdout` are printed only on success.
pub fn dispatch(
    prog: &str,
    env: &Environment,
    paths: &Paths,
    style: &Style,
    parsed: &Parsed,
    stdout: &mut Vec<String>,
) -> Option<Result<(), Error>> {
    let mut commands = Commands { prog, env, stdout };
    let json = parsed.flag("--json");
    Some(if parsed.flag("--status") {
        commands.status(paths, style, json)
    } else if parsed.flag("--list") {
        commands.list(paths, style, json, parsed.flag("--token-status"))
    } else if parsed.flag("--add-account") {
        commands.add(paths, style, parsed)
    } else if let Some(destination) = parsed.value("--export") {
        commands.export(paths, destination, parsed)
    } else if let Some(source) = parsed.value("--import") {
        commands.import(paths, source, parsed.flag("--force"))
    } else if let Some(token) = parsed.value("--add-token") {
        commands.add_token(paths, style, token, parsed)
    } else if parsed.flag("--switch") || nonempty(parsed, "--switch-to") {
        commands.switch(paths, style, parsed)
    } else {
        return crate::claude::manage::dispatch(env, paths, style, parsed);
    })
}

struct Commands<'a> {
    prog: &'a str,
    env: &'a Environment,
    stdout: &'a mut Vec<String>,
}

impl Commands<'_> {
    fn status(&mut self, paths: &Paths, style: &Style, json: bool) -> Result<(), Error> {
        let endpoints = Endpoints::new(self.env);
        let keychain = Keychain::new(endpoints.security.clone(), self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), self.env);
        let client = AnthropicClient::new(endpoints);
        let report = status::status(paths, self.env, &secrets, &client)?;
        if json {
            self.stdout
                .push(pyjson::to_string_pretty(&status::status_json(&report)));
        } else {
            self.stdout.extend(status::status_lines(&report, style));
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
        let endpoints = Endpoints::new(self.env);
        let keychain = Keychain::new(endpoints.security.clone(), self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), self.env);
        let client = AnthropicClient::new(endpoints);
        let options = ListOptions { token_status, json };
        let report = listing::list(paths, self.env, &secrets, &client, options)?;
        if json {
            self.stdout
                .push(pyjson::to_string_pretty(&listing::list_json(&report)));
            return Ok(());
        }
        if report == ListReport::NoStore {
            TerminalConsole.line(&style.dimmed("No accounts are managed yet."));
            return self.first_run_setup(paths, style, &secrets, &client);
        }
        self.stdout.extend(listing::list_lines(&report, style));
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
        let Some(identity) = ccswap_claude::store::current_identity(paths) else {
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
            env: self.env,
            secrets,
            client,
            style,
        };
        accounts.add_account(&AddOptions::default(), &mut console)
    }

    fn add(&mut self, paths: &Paths, style: &Style, parsed: &Parsed) -> Result<(), Error> {
        let endpoints = Endpoints::new(self.env);
        let keychain = Keychain::new(endpoints.security.clone(), self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), self.env);
        let client = AnthropicClient::new(endpoints);
        let accounts = Accounts {
            paths,
            env: self.env,
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
        let keychain = Keychain::new(Endpoints::new(self.env).security, self.env);
        SecretStore::new(keychain, paths.clone(), self.env)
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
        let client = AnthropicClient::new(Endpoints::new(self.env));
        let env = self.env;
        let show_list = |console: &mut dyn Console| -> Result<(), Error> {
            let list_secrets = self.secrets(paths);
            let report = listing::list(paths, env, &list_secrets, &client, ListOptions::default())?;
            for line in listing::list_lines(&report, style) {
                console.line(&line);
            }
            Ok(())
        };
        let switcher = Switcher {
            paths,
            env: self.env,
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
        let endpoints = Endpoints::new(self.env);
        let keychain = Keychain::new(endpoints.security.clone(), self.env);
        let secrets = SecretStore::new(keychain, paths.clone(), self.env);
        let client = AnthropicClient::new(endpoints);
        let accounts = Accounts {
            paths,
            env: self.env,
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
}

fn switch_strategy(paths: &Paths, parsed: &Parsed) -> Option<SwitchStrategy> {
    let mut chosen = SwitchStrategy::new(Strategy::parse(parsed.value("--strategy")?)?);
    if let Some(names) = parsed.value("--model") {
        chosen.models = parse_model_names(names);
        chosen.model_source = Some(ModelSource::Cli);
    } else {
        chosen.models = autoswitch_models(&Settings::new(paths, ccswap_claude::SETTINGS));
        chosen.model_source = (!chosen.models.is_empty()).then_some(ModelSource::Setting);
    }
    Some(chosen)
}
