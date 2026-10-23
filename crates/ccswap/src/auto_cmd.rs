use std::io::Write;
use std::sync::atomic::Ordering;

use ccswap_claude::anthropic_client::AnthropicClient;
use ccswap_claude::autoswitch::{
    AutoStrategy, AutoSwitchEngine, AutoSwitchEvent, AutoSwitchSettings, CliOverrides, LoopHandle,
    SwitcherHost,
};
use ccswap_claude::secret_store::{Keychain, SecretStore};
use ccswap_core::endpoints::Endpoints;
use ccswap_core::env::{Environment, is_root};
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;
use ccswap_core::pyjson;
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
    let once = parsed.flag("--once");
    let dry_run = parsed.flag("--dry-run");

    let stopped = format!("\n{}", style.dimmed("Auto-switch stopped"));
    let report_stopped = move || {
        if json {
            eprintln!("{stopped}");
        } else {
            print_flushed(&stopped);
        }
    };
    let handle = LoopHandle::new();
    let interrupted = signals::route_to(handle.clone(), !once, {
        let report_stopped = report_stopped.clone();
        move || {
            report_stopped();
            std::process::exit(130);
        }
    });

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
    .dry_run(dry_run)
    .loop_handle(handle);

    let code = if once {
        engine.tick().exit_code()
    } else {
        if !json {
            let settings = engine.settings();
            print_flushed(&style.dimmed(&format!(
                "Auto-switch running: threshold {:.0}%, every {:.0}s{} — Ctrl-C to stop",
                settings.threshold,
                settings.interval_seconds,
                if dry_run { " (dry-run)" } else { "" }
            )));
        }
        engine.run_loop()
    };
    if interrupted.load(Ordering::SeqCst) {
        report_stopped();
        return 130;
    }
    code
}

/// Signal handlers cannot take the loop's lock, so they only write the
/// signal number to a pipe that a watcher thread turns into `stop()`.
mod signals {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    use ccswap_claude::autoswitch::LoopHandle;

    static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn forward(sig: libc::c_int) {
        let fd = PIPE_WRITE.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = sig as u8;
            unsafe {
                libc::write(fd, (&byte as *const u8).cast(), 1);
            }
        }
    }

    /// SIGINT (and SIGTERM with `sigterm`) stop `handle` after the tick in
    /// flight; the returned flag says whether SIGINT came. A second SIGINT
    /// runs `on_second_interrupt`.
    pub fn route_to(
        handle: LoopHandle,
        sigterm: bool,
        on_second_interrupt: impl Fn() + Send + 'static,
    ) -> Arc<AtomicBool> {
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return interrupted;
        }
        for fd in fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        PIPE_WRITE.store(fds[1], Ordering::SeqCst);
        let flag = interrupted.clone();
        std::thread::spawn(move || {
            let mut pipe = unsafe { File::from_raw_fd(fds[0]) };
            let mut byte = [0u8; 1];
            while pipe.read_exact(&mut byte).is_ok() {
                if libc::c_int::from(byte[0]) == libc::SIGINT && flag.swap(true, Ordering::SeqCst) {
                    on_second_interrupt();
                }
                handle.stop();
            }
        });
        let signals: &[libc::c_int] = if sigterm {
            &[libc::SIGINT, libc::SIGTERM]
        } else {
            &[libc::SIGINT]
        };
        for &sig in signals {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = forward as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
        interrupted
    }
}
