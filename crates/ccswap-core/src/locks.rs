//! Cross-process locks, interoperable with Python `cswap` and Claude Code.
//!
//! [`StoreLock`] is a BSD `flock(2)` on a lock file (what Python's
//! `fcntl.flock` takes; POSIX record locks would not exclude it).
//! [`DirLock`] is npm proper-lockfile's protocol, which Claude Code uses: the
//! lock is a directory, stale by mtime, touched while held.
//!
//! Global acquisition order, never inverted:
//! `.consume-<n>.lock` → store `.lock` → `.oauth_refresh.lock` →
//! `~/.claude.lock` → `~/.claude.json.lock`.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::error::{Error, ErrorKind, Result};
use crate::paths::Paths;

pub const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const STORE_LOCK_POLL: Duration = Duration::from_millis(100);

pub const CREDENTIALS_STALENESS: Duration = Duration::from_secs(60);
pub const CONFIG_STALENESS: Duration = Duration::from_secs(10);
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(3);
pub const DIR_LOCK_TIMEOUT: Duration = Duration::from_secs(9);

/// Exclusive `flock` on a file; released on drop. Not reentrant: a second
/// acquisition of the same path in one process waits for the first.
#[derive(Debug)]
pub struct StoreLock {
    file: File,
}

impl StoreLock {
    /// Waits up to `timeout`; `Ok(None)` when it stayed held.
    pub fn try_acquire(path: &Path, timeout: Duration) -> std::io::Result<Option<StoreLock>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let start = Instant::now();
        loop {
            // SAFETY: flock on a descriptor we own.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(StoreLock { file }));
            }
            if start.elapsed() > timeout {
                return Ok(None);
            }
            std::thread::sleep(STORE_LOCK_POLL);
        }
    }

    /// Like `FileLock` used as a context manager: a timeout is a `LockError`.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<StoreLock> {
        match Self::try_acquire(path, timeout) {
            Ok(Some(lock)) => Ok(lock),
            Ok(None) => Err(lock_error()),
            Err(e) => Err(Error::new(
                ErrorKind::LockError,
                format!("Failed to acquire lock {}: {e}", path.display()),
            )),
        }
    }
}

fn lock_error() -> Error {
    Error::new(
        ErrorKind::LockError,
        "Failed to acquire lock - another instance may be running",
    )
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // SAFETY: as above; closing the file would release it anyway.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DirLockOptions {
    pub timeout: Duration,
    pub staleness: Duration,
    pub touch_interval: Duration,
}

impl DirLockOptions {
    pub fn credentials() -> Self {
        DirLockOptions {
            timeout: DIR_LOCK_TIMEOUT,
            staleness: CREDENTIALS_STALENESS,
            touch_interval: TOUCH_INTERVAL,
        }
    }

    pub fn config() -> Self {
        DirLockOptions {
            staleness: CONFIG_STALENESS,
            ..Self::credentials()
        }
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        DirLockOptions { timeout, ..self }
    }
}

/// A proper-lockfile directory lock; removed on drop.
#[derive(Debug)]
pub struct DirLock {
    dir: PathBuf,
    stop: Option<Sender<()>>,
    toucher: Option<JoinHandle<()>>,
}

impl DirLock {
    pub fn acquire(dir: &Path, options: DirLockOptions) -> Result<DirLock> {
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = Instant::now();
        loop {
            match std::fs::create_dir(dir) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(Error::new(
                        ErrorKind::LockError,
                        format!("Could not create {}: {e}", dir.display()),
                    ));
                }
            }
            if start.elapsed() > options.timeout {
                return Err(Error::new(
                    ErrorKind::ClaudeCodeLockTimeout,
                    format!(
                        "Could not acquire {} — Claude Code appears to be refreshing credentials. Retry in a few seconds.",
                        dir.file_name().unwrap_or_default().to_string_lossy()
                    ),
                ));
            }
            let mtime = match std::fs::metadata(dir).and_then(|m| m.modified()) {
                Ok(mtime) => mtime,
                Err(_) => continue,
            };
            let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
            if age > options.staleness {
                if std::fs::remove_dir(dir).is_err() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
            std::thread::sleep(Duration::from_millis(250) + jitter(Duration::from_millis(250)));
        }

        let (stop, stopped) = mpsc::channel::<()>();
        let touched = dir.to_path_buf();
        let interval = options.touch_interval;
        let toucher = std::thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                if touch(&touched).is_err() {
                    return;
                }
            }
        });
        Ok(DirLock {
            dir: dir.to_path_buf(),
            stop: Some(stop),
            toucher: Some(toucher),
        })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(toucher) = self.toucher.take() {
            let _ = toucher.join();
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

pub fn touch(path: &Path) -> std::io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: valid C string; a null times pointer means "now".
    if unsafe { libc::utimes(c.as_ptr(), std::ptr::null()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn jitter(max: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    max.mul_f64(f64::from(nanos % 1000) / 1000.0)
}

/// Claude Code's credential-refresh pair, in its order: the primary
/// `.oauth_refresh.lock`, then the legacy `<config_home>.lock`. A legacy
/// timeout releases the primary (dropped on the error path).
#[derive(Debug)]
pub struct ClaudeCredentialsLock {
    _legacy: DirLock,
    _primary: DirLock,
}

pub fn claude_credentials_lock(
    paths: &Paths,
    timeout: Option<Duration>,
) -> Result<ClaudeCredentialsLock> {
    let mut options = DirLockOptions::credentials();
    if let Some(timeout) = timeout {
        options = options.timeout(timeout);
    }
    let primary = DirLock::acquire(&paths.oauth_refresh_lock_dir(), options)?;
    let legacy = DirLock::acquire(&paths.credentials_lock_dir(), options)?;
    Ok(ClaudeCredentialsLock {
        _legacy: legacy,
        _primary: primary,
    })
}

/// Claude Code's global-config write lock (`~/.claude.json.lock`).
pub fn claude_config_lock(paths: &Paths, timeout: Option<Duration>) -> Result<DirLock> {
    let mut options = DirLockOptions::config();
    if let Some(timeout) = timeout {
        options = options.timeout(timeout);
    }
    DirLock::acquire(&paths.config_lock_dir(), options)
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_locking.py and tests/test_claude_locks.py.
    use super::*;
    use crate::env::Environment;

    fn set_mtime_ago(path: &Path, secs: i64) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let tv = libc::timeval {
            tv_sec: now - secs,
            tv_usec: 0,
        };
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), [tv, tv].as_ptr()) }, 0);
    }

    fn age(path: &Path) -> Duration {
        SystemTime::now()
            .duration_since(std::fs::metadata(path).unwrap().modified().unwrap())
            .unwrap_or_default()
    }

    fn home_paths(home: &Path) -> Paths {
        Paths::new(&Environment::from_pairs([("HOME", home.as_os_str())]))
    }

    const SHORT: Duration = Duration::from_millis(500);

    #[test]
    fn store_lock_acquire_creates_parents_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/.lock");
        let lock = StoreLock::acquire(&path, SHORT).unwrap();
        assert!(path.exists());
        drop(lock);
        let again = StoreLock::try_acquire(&path, SHORT).unwrap();
        assert!(again.is_some(), "acquired after release");
        assert!(path.exists(), "the lock file is never deleted");
    }

    #[test]
    fn store_lock_times_out_while_held_and_is_not_reentrant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let _held = StoreLock::acquire(&path, SHORT).unwrap();
        let start = Instant::now();
        assert!(StoreLock::try_acquire(&path, SHORT).unwrap().is_none());
        assert!(start.elapsed() >= SHORT);
        let err = StoreLock::acquire(&path, SHORT).unwrap_err();
        assert_eq!(err.kind, ErrorKind::LockError);
        assert_eq!(
            err.message,
            "Failed to acquire lock - another instance may be running"
        );
    }

    #[test]
    fn store_lock_is_a_bsd_flock_seen_by_other_open_files() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let held = StoreLock::acquire(&path, SHORT).unwrap();
        // flock(2) locks belong to the open file description, so a second open contends like another process.
        let other = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let flock = || unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(flock(), -1, "a second open file must not get the lock");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );
        drop(held);
        assert_eq!(flock(), 0, "the lock is free once released");
    }

    #[test]
    fn dir_lock_is_created_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("a/b/target.lock");
        {
            let _lock = DirLock::acquire(&lock_dir, DirLockOptions::config()).unwrap();
            assert!(lock_dir.is_dir());
        }
        assert!(!lock_dir.exists());
        let _again = DirLock::acquire(&lock_dir, DirLockOptions::config()).unwrap();
        assert!(lock_dir.is_dir());
    }

    #[test]
    fn dir_lock_contention_times_out_and_leaves_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("target.lock");
        std::fs::create_dir(&lock_dir).unwrap();
        let start = Instant::now();
        let err = DirLock::acquire(&lock_dir, DirLockOptions::config().timeout(SHORT)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ClaudeCodeLockTimeout);
        assert_eq!(
            err.message,
            "Could not acquire target.lock — Claude Code appears to be refreshing credentials. Retry in a few seconds."
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(lock_dir.is_dir());
    }

    #[test]
    fn dir_lock_takes_over_a_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("target.lock");
        std::fs::create_dir(&lock_dir).unwrap();
        set_mtime_ago(&lock_dir, 30);
        {
            let _lock = DirLock::acquire(
                &lock_dir,
                DirLockOptions::config().timeout(Duration::from_secs(2)),
            )
            .unwrap();
            assert!(age(&lock_dir) < Duration::from_secs(5));
        }
        assert!(!lock_dir.exists());
    }

    #[test]
    fn dir_lock_release_tolerates_a_stolen_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("target.lock");
        {
            let _lock = DirLock::acquire(&lock_dir, DirLockOptions::config()).unwrap();
            std::fs::remove_dir(&lock_dir).unwrap();
        }
        assert!(!lock_dir.exists());
    }

    #[test]
    fn dir_lock_toucher_keeps_the_mtime_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join("target.lock");
        let options = DirLockOptions {
            touch_interval: Duration::from_millis(100),
            ..DirLockOptions::config()
        };
        let _lock = DirLock::acquire(&lock_dir, options).unwrap();
        set_mtime_ago(&lock_dir, 30);
        std::thread::sleep(Duration::from_millis(400));
        assert!(age(&lock_dir) < Duration::from_secs(10));
    }

    #[test]
    fn lock_paths_follow_claude_config_dir() {
        let p = home_paths(Path::new("/h"));
        assert_eq!(p.credentials_lock_dir(), PathBuf::from("/h/.claude.lock"));
        assert_eq!(
            p.oauth_refresh_lock_dir(),
            PathBuf::from("/h/.claude/.oauth_refresh.lock")
        );
        assert_eq!(p.config_lock_dir(), PathBuf::from("/h/.claude.json.lock"));
        let p = Paths::new(&Environment::from_pairs([
            ("HOME", "/h"),
            ("CLAUDE_CONFIG_DIR", "/t/custom-claude"),
        ]));
        assert_eq!(
            p.credentials_lock_dir(),
            PathBuf::from("/t/custom-claude.lock")
        );
        assert_eq!(
            p.oauth_refresh_lock_dir(),
            PathBuf::from("/t/custom-claude/.oauth_refresh.lock")
        );
        assert_eq!(
            p.config_lock_dir(),
            PathBuf::from("/t/custom-claude/.claude.json.lock")
        );
    }

    #[test]
    fn credentials_lock_takes_both_and_config_lock_nests() {
        let home = tempfile::tempdir().unwrap();
        let p = home_paths(home.path());
        {
            let _creds = claude_credentials_lock(&p, None).unwrap();
            assert!(home.path().join(".claude/.oauth_refresh.lock").is_dir());
            assert!(home.path().join(".claude.lock").is_dir());
            let _config = claude_config_lock(&p, None).unwrap();
            assert!(home.path().join(".claude.json.lock").is_dir());
        }
        assert!(!home.path().join(".claude/.oauth_refresh.lock").exists());
        assert!(!home.path().join(".claude.lock").exists());
        assert!(!home.path().join(".claude.json.lock").exists());
    }

    #[test]
    fn primary_contention_never_touches_the_legacy_lock() {
        let home = tempfile::tempdir().unwrap();
        let primary = home.path().join(".claude/.oauth_refresh.lock");
        std::fs::create_dir_all(&primary).unwrap();
        let err = claude_credentials_lock(&home_paths(home.path()), Some(SHORT)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ClaudeCodeLockTimeout);
        assert!(!home.path().join(".claude.lock").exists());
        assert!(primary.is_dir());
    }

    #[test]
    fn legacy_contention_releases_the_primary() {
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".claude.lock");
        std::fs::create_dir_all(&legacy).unwrap();
        claude_credentials_lock(&home_paths(home.path()), Some(SHORT)).unwrap_err();
        assert!(!home.path().join(".claude/.oauth_refresh.lock").exists());
        assert!(legacy.is_dir());
    }

    #[test]
    fn credential_locks_are_stale_only_after_60s() {
        let home = tempfile::tempdir().unwrap();
        let p = home_paths(home.path());
        let primary = home.path().join(".claude/.oauth_refresh.lock");
        std::fs::create_dir_all(&primary).unwrap();
        set_mtime_ago(&primary, 30);
        claude_credentials_lock(&p, Some(SHORT)).unwrap_err();
        assert!(primary.is_dir());

        let legacy = home.path().join(".claude.lock");
        std::fs::create_dir_all(&legacy).unwrap();
        set_mtime_ago(&primary, 70);
        set_mtime_ago(&legacy, 70);
        drop(claude_credentials_lock(&p, Some(Duration::from_secs(2))).unwrap());
        assert!(!primary.exists() && !legacy.exists());
    }

    #[test]
    fn config_lock_is_stale_after_10s() {
        let home = tempfile::tempdir().unwrap();
        let cfg = home.path().join(".claude.json.lock");
        std::fs::create_dir(&cfg).unwrap();
        set_mtime_ago(&cfg, 30);
        drop(claude_config_lock(&home_paths(home.path()), Some(Duration::from_secs(2))).unwrap());
        assert!(!cfg.exists());
    }
}
