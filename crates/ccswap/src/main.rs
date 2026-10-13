mod argparse;
mod auto_cmd;
mod cli;
mod launch_agent;
mod config_cmd;
mod manage_cmd;
mod map_cmd;
mod menubar;
mod renumber_cmd;
mod run_cmd;
mod tui;
mod unclaimed_cmd;

use ccswap_core::env::Environment;

fn main() {
    let mut argv = std::env::args();
    let prog = cli::prog_name(argv.next().as_deref());
    let code = cli::Cli::new(prog, Environment::from_process()).run(argv.collect());
    std::process::exit(code);
}
