use ccswap_core::env::Environment;
use ccswap_core::error::Error;
use ccswap_core::presentation::Style;

use crate::cli::argparse::Parsed;
use crate::ui::launch_agent::{self, LaunchAgent};

pub fn run(prog: &str, env: &Environment, parsed: &Parsed, style: &Style) -> Result<(), Error> {
    let agent = LaunchAgent::new(env);
    if parsed.flag("--install-service") {
        let program = launch_agent::resolve_program(
            std::env::args_os().next().as_deref(),
            &std::env::current_dir().unwrap_or_default(),
            ccswap_core::path_search::which(env, launch_agent::PROGRAM_NAME),
            || std::env::current_exe().ok(),
        );
        let installed = agent.install(program)?;
        println!("Menu bar service installed ({}).", installed.label);
        println!("  plist: {}", installed.plist.display());
        println!("  logs:  {}", installed.stderr_log.display());
        println!(
            "{}",
            style.dimmed(&format!(
                "It starts at login from now on. Re-run this after a {prog} upgrade to point launchd at the new build."
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
                "Install it with: {prog} menubar --install-service"
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
