//! `ccswap purge`: removes every provider's ccswap data. The Codex store lives
//! under the backup root, so removing that directory covers it; the live
//! `~/.codex/auth.json` is a copy, never a link into the store.

use ccswap_claude::accounts::Accounts;
use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::console::Console;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::Environment;
use ccswap_core::error::Error;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;

pub fn run(
    env: &Environment,
    paths: &Paths,
    style: &Style,
    console: &mut dyn Console,
) -> Result<(), Error> {
    let endpoints = Endpoints::new(env);
    let keychain = Keychain::new(endpoints.security.clone(), env);
    let secrets = SecretStore::new(keychain, paths.clone(), env);
    let client = AnthropicClient::new(endpoints);
    let accounts = Accounts {
        paths,
        env,
        secrets: &secrets,
        client: &client,
        style,
    };
    let claude = accounts.prepare_purge()?;
    let backup_root = paths.backup_root();

    console.line(&style.yellowed(
        "This will remove ALL ccswap data, for both Claude Code and Codex, from your system:",
    ));
    console.line(&format!("  - Backup directory: {}", backup_root.display()));
    console.line("  - All stored account credentials (macOS Keychain and/or files)");
    if claude.has_session_profiles() {
        console.line("  - All session profiles and their Keychain entries");
    }
    console.line("");
    console.line(
        &style.dimmed("Note: This does NOT affect your current Claude Code or Codex CLI login."),
    );
    console.line("");
    match console.prompt("Are you sure you want to purge all data? [y/N] ") {
        Some(answer) if answer.to_lowercase() == "y" => {}
        Some(_) => {
            console.line(&style.dimmed("Cancelled"));
            return Ok(());
        }
        None => {
            console.line(&format!("\n{}", style.dimmed("Cancelled")));
            return Ok(());
        }
    }

    let mut removed = accounts.purge_credentials(&claude)?;
    if backup_root.exists() {
        std::fs::remove_dir_all(&backup_root).map_err(|e| {
            Error::config(format!("Could not remove {}: {e}", backup_root.display()))
        })?;
        removed.push(format!("Directory: {}", backup_root.display()));
    }

    if removed.is_empty() {
        console.line(&format!(
            "\n{}",
            style.dimmed("No ccswap data found to remove.")
        ));
    } else {
        console.line(&format!("\n{}", style.accent("Removed:")));
        for item in &removed {
            console.line(&format!("  {} {item}", style.dimmed("-")));
        }
    }
    console.line(&format!("\n{}", style.accent("Purge complete.")));
    Ok(())
}
