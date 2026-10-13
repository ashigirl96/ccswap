mod argparse;
mod cli;

use ccswap_core::env::Environment;

fn main() {
    let mut argv = std::env::args();
    let prog = cli::prog_name(argv.next().as_deref());
    let code = cli::Cli::new(prog, Environment::from_process()).run(argv.collect());
    std::process::exit(code);
}
