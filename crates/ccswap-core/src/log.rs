//! The process-wide `claude-swap` logger (upstream `logging_config.py`):
//! `<backup_root>/claude-swap.log`, rotated at 1 MiB with 3 backups, shared
//! with Python `cswap`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_FILENAME: &str = "claude-swap.log";
const MAX_BYTES: u64 = 1024 * 1024;
const BACKUP_COUNT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warning,
    Error,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }
}

struct Config {
    file: PathBuf,
    debug: bool,
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Points the logger at `<backup_root>/claude-swap.log`. Nothing is created
/// until the first record is written.
pub fn setup(backup_root: &Path, debug: bool) {
    *lock() = Some(Config {
        file: backup_root.join(LOG_FILENAME),
        debug,
    });
}

pub fn debug(message: impl AsRef<str>) {
    emit(Level::Debug, message.as_ref());
}

pub fn info(message: impl AsRef<str>) {
    emit(Level::Info, message.as_ref());
}

pub fn warning(message: impl AsRef<str>) {
    emit(Level::Warning, message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    emit(Level::Error, message.as_ref());
}

fn lock() -> std::sync::MutexGuard<'static, Option<Config>> {
    CONFIG.lock().unwrap_or_else(|e| e.into_inner())
}

fn emit(level: Level, message: &str) {
    let guard = lock();
    let Some(config) = guard.as_ref() else {
        // Python's last-resort handler: bare warnings and errors on stderr.
        if level >= Level::Warning {
            eprintln!("{message}");
        }
        return;
    };
    if level < Level::Info && !config.debug {
        return;
    }
    let line = format!(
        "{} - {} - {message}\n",
        asctime(SystemTime::now()),
        level.name()
    );
    // A log that cannot be written never fails the command.
    let _ = append(&config.file, &line);
    if config.debug {
        eprintln!("{}: {message}", level.name());
    }
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if should_roll_over(file, line) {
        roll_over(file)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(line.as_bytes())
}

/// Python's `RotatingFileHandler.shouldRollover`, which measures the record
/// in characters and never rotates an empty file.
fn should_roll_over(file: &Path, line: &str) -> bool {
    match std::fs::metadata(file) {
        Ok(meta) if meta.is_file() => {
            let pos = meta.len();
            pos > 0 && pos + line.chars().count() as u64 >= MAX_BYTES
        }
        _ => false,
    }
}

fn roll_over(file: &Path) -> std::io::Result<()> {
    let numbered = |i: u32| {
        let mut name = file.as_os_str().to_owned();
        name.push(format!(".{i}"));
        PathBuf::from(name)
    };
    for i in (1..BACKUP_COUNT).rev() {
        let (source, dest) = (numbered(i), numbered(i + 1));
        if source.exists() {
            if dest.exists() {
                std::fs::remove_file(&dest)?;
            }
            std::fs::rename(&source, &dest)?;
        }
    }
    let first = numbered(1);
    if first.exists() {
        std::fs::remove_file(&first)?;
    }
    if file.exists() {
        std::fs::rename(file, &first)?;
    }
    Ok(())
}

/// Python's default `asctime`: local `YYYY-MM-DD HH:MM:SS,mmm`.
fn asctime(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed `tm` we pass it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        since.subsec_millis()
    )
}
