//! The label is cswap's, so installing replaces a Python cswap service.

mod plist;

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ccswap_core::env::Environment;
use ccswap_core::error::{Error, ErrorKind};

pub use plist::Plist;

pub const LABEL: &str = "com.cswap.menubar";

pub const PROGRAM_NAME: &str = "ccswap";
pub const BUNDLE_ID: &str = "io.github.ashigirl96.ccswap";
const EXTRA_PATH_DIRS: [&str; 3] = ["~/.local/bin", "/opt/homebrew/bin", "/usr/local/bin"];
const BASE_PATH_DIRS: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];
// `bootout` can return before launchd has torn the job down, and a
// `bootstrap` inside that window fails with "Operation already in progress".
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(5);
const UNLOAD_POLL: Duration = Duration::from_millis(100);

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn log_paths(home: &Path) -> (PathBuf, PathBuf) {
    let logs = home.join("Library/Logs");
    (
        logs.join(format!("{LABEL}.log")),
        logs.join(format!("{LABEL}.err")),
    )
}

/// `argv[0]` is not symlink-resolved, so a symlinked install keeps its name
/// across upgrades while its target moves.
pub fn resolve_program(
    argv0: Option<&OsStr>,
    cwd: &Path,
    on_path: Option<PathBuf>,
    current_exe: impl FnOnce() -> Option<PathBuf>,
) -> Vec<String> {
    let absolute = |p: &Path| normalize(&cwd.join(p));
    if let Some(argv0) = argv0.filter(|a| !a.is_empty()) {
        let candidate = absolute(Path::new(argv0));
        if candidate.file_name() == Some(OsStr::new(PROGRAM_NAME)) && candidate.is_file() {
            return vec![candidate.display().to_string()];
        }
    }
    if let Some(found) = on_path {
        return vec![absolute(&found).display().to_string()];
    }
    vec![
        current_exe()
            .unwrap_or_else(|| PathBuf::from(PROGRAM_NAME))
            .display()
            .to_string(),
    ]
}

/// Lexical like Python's `os.path.normpath`: `.` and `..` go, symlinks stay.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn path_env(program: &[String], home: &Path) -> String {
    let mut dirs: Vec<String> = Vec::new();
    if let Some(first) = program.first().and_then(|p| Path::new(p).parent()) {
        let first = first.display().to_string();
        if !first.is_empty() && first != "." {
            dirs.push(first);
        }
    }
    for dir in EXTRA_PATH_DIRS.iter().chain(BASE_PATH_DIRS.iter()) {
        let expanded = match dir.strip_prefix("~/") {
            Some(rest) => home.join(rest).display().to_string(),
            None => dir.to_string(),
        };
        if !dirs.contains(&expanded) {
            dirs.push(expanded);
        }
    }
    dirs.join(":")
}

pub fn build_plist(program: &[String], home: &Path) -> Plist {
    let (out_log, err_log) = log_paths(home);
    let mut arguments: Vec<Plist> = program.iter().map(Plist::string).collect();
    arguments.push(Plist::string("menubar"));
    Plist::dict([
        ("Label", Plist::string(LABEL)),
        ("ProgramArguments", Plist::Array(arguments)),
        ("RunAtLoad", Plist::Bool(true)),
        // Restart a crash but respect Quit, which exits 0.
        (
            "KeepAlive",
            Plist::dict([("SuccessfulExit", Plist::Bool(false))]),
        ),
        // Background would get throttled I/O and CPU for a UI process.
        ("ProcessType", Plist::string("Interactive")),
        (
            "EnvironmentVariables",
            Plist::dict([("PATH", Plist::string(path_env(program, home)))]),
        ),
        (
            "StandardOutPath",
            Plist::string(out_log.display().to_string()),
        ),
        (
            "StandardErrorPath",
            Plist::string(err_log.display().to_string()),
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub label: String,
    pub plist: PathBuf,
    pub program: Vec<String>,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uninstalled {
    pub was_loaded: bool,
    pub removed_plist: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
    pub state: Option<String>,
    pub pid: Option<u64>,
    pub plist: PathBuf,
}

/// The job's own `state` and `pid` from `launchctl print`. Nested blocks
/// (endpoints, environment) repeat those keys one tab deeper.
fn parse_print(stdout: &str) -> (Option<String>, Option<u64>) {
    let mut state = None;
    let mut pid = None;
    for line in stdout.lines() {
        if !line.starts_with('\t') || line.starts_with("\t\t") {
            continue;
        }
        let field = line.trim();
        if let (None, Some(value)) = (&state, field.strip_prefix("state = ")) {
            state = Some(value.trim().to_string());
        } else if let (None, Some(raw)) = (pid, field.strip_prefix("pid = ").map(str::trim))
            && !raw.is_empty()
            && raw.bytes().all(|b| b.is_ascii_digit())
        {
            pid = raw.parse().ok();
        }
    }
    (state, pid)
}

struct Reply {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Reply {
    fn detail(&self) -> String {
        let text = if self.stderr.is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        text.trim().to_string()
    }
}

pub struct LaunchAgent {
    env: Environment,
    home: PathBuf,
    uid: u32,
}

impl LaunchAgent {
    pub fn new(env: &Environment) -> Self {
        LaunchAgent {
            env: env.clone(),
            home: env.home(),
            uid: unsafe { libc::getuid() },
        }
    }

    fn service_target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }

    fn domain_target(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn launchctl(&self, args: &[&str]) -> Result<Reply, Error> {
        let not_found = || {
            Error::new(
                ErrorKind::ClaudeSwitchError,
                "launchctl not found; is this macOS?",
            )
        };
        let program =
            ccswap_core::path_search::which(&self.env, "launchctl").ok_or_else(not_found)?;
        let out = Command::new(program)
            .args(args)
            .output()
            .map_err(|_| not_found())?;
        Ok(Reply {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn is_loaded(&self) -> Result<bool, Error> {
        Ok(self.launchctl(&["print", &self.service_target()])?.code == 0)
    }

    fn wait_until_unloaded(&self) -> Result<bool, Error> {
        let deadline = Instant::now() + UNLOAD_TIMEOUT;
        while self.is_loaded()? {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(UNLOAD_POLL);
        }
        Ok(true)
    }

    /// Boots out a loaded job first so a reinstall re-reads the plist.
    pub fn install(&self, program: Vec<String>) -> Result<Installed, Error> {
        let plist = plist_path(&self.home);
        let (out_log, err_log) = log_paths(&self.home);
        for dir in [plist.parent(), out_log.parent()].into_iter().flatten() {
            std::fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
        }
        std::fs::write(&plist, build_plist(&program, &self.home).to_xml())
            .map_err(|e| io_error(&plist, e))?;

        let mut settled = true;
        if self.is_loaded()? {
            self.launchctl(&["bootout", &self.service_target()])?;
            settled = self.wait_until_unloaded()?;
        }

        let plist_arg = plist.display().to_string();
        let booted = self.launchctl(&["bootstrap", &self.domain_target(), &plist_arg])?;
        if booted.code != 0 {
            let mut detail = booted.detail();
            if !settled {
                detail = format!("{detail}; the previous instance was still shutting down")
                    .trim_start_matches([';', ' '])
                    .to_string();
            }
            return Err(failure("bootstrap", booted.code, &detail));
        }

        let mut program = program;
        program.push("menubar".into());
        Ok(Installed {
            label: LABEL.into(),
            plist,
            program,
            stdout_log: out_log,
            stderr_log: err_log,
        })
    }

    /// Arrives at "gone" from any partial state: loaded without a plist, a
    /// plist never bootstrapped, or neither.
    pub fn uninstall(&self) -> Result<Uninstalled, Error> {
        ccswap::openai_mode::disable(&ccswap_core::paths::Paths::new(&self.env))?;
        let plist = plist_path(&self.home);
        let was_loaded = self.is_loaded()?;
        if was_loaded {
            let booted_out = self.launchctl(&["bootout", &self.service_target()])?;
            if booted_out.code != 0 && self.is_loaded()? {
                return Err(failure("bootout", booted_out.code, &booted_out.detail()));
            }
        }
        let removed_plist = plist.exists();
        if removed_plist {
            std::fs::remove_file(&plist).map_err(|e| io_error(&plist, e))?;
        }
        Ok(Uninstalled {
            was_loaded,
            removed_plist,
        })
    }

    /// Starts the loaded job unless it is already running; `false` when
    /// launchd could not, such as when the service is not installed.
    pub fn kickstart(&self) -> bool {
        self.launchctl(&["kickstart", &self.service_target()])
            .is_ok_and(|reply| reply.code == 0)
    }

    pub fn status(&self) -> Result<ServiceStatus, Error> {
        let printed = self.launchctl(&["print", &self.service_target()])?;
        let loaded = printed.code == 0;
        let (state, pid) = if loaded {
            parse_print(&printed.stdout)
        } else {
            (None, None)
        };
        let plist = plist_path(&self.home);
        Ok(ServiceStatus {
            installed: plist.exists(),
            loaded,
            state,
            pid,
            plist,
        })
    }
}

fn failure(verb: &str, code: i32, detail: &str) -> Error {
    let mut message = format!("launchctl {verb} failed (exit {code})");
    if !detail.is_empty() {
        message.push_str(&format!(": {detail}"));
    }
    Error::new(ErrorKind::ClaudeSwitchError, message)
}

fn io_error(path: &Path, e: std::io::Error) -> Error {
    Error::new(
        ErrorKind::ClaudeSwitchError,
        format!("{}: {e}", path.display()),
    )
}

#[cfg(test)]
mod tests;
