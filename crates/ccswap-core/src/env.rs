use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// The process environment, captured once so every module resolves paths,
/// endpoints and the Keychain account from the same snapshot.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    vars: BTreeMap<String, OsString>,
}

impl Environment {
    pub fn from_process() -> Self {
        let vars = std::env::vars_os()
            .filter_map(|(k, v)| k.into_string().ok().map(|k| (k, v)))
            .collect();
        Environment { vars }
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<OsString>,
    {
        Environment {
            vars: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// Defined, possibly empty.
    pub fn get(&self, name: &str) -> Option<&OsString> {
        self.vars.get(name)
    }

    /// Defined and non-empty, as a string.
    pub fn non_empty(&self, name: &str) -> Option<String> {
        self.vars
            .get(name)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string_lossy().into_owned())
    }

    pub fn vars(&self) -> impl Iterator<Item = (&str, &OsString)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn is_set(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    /// `$HOME`, which Python's `Path.home()` also prefers over the passwd entry.
    pub fn home(&self) -> PathBuf {
        match self.vars.get("HOME").filter(|v| !v.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => passwd_home().unwrap_or_else(|| PathBuf::from("/")),
        }
    }
}

fn passwd_home() -> Option<PathBuf> {
    // SAFETY: getpwuid returns a pointer into static storage or null; we copy
    // the string out before any other passwd call can overwrite it.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
        Some(PathBuf::from(dir.to_string_lossy().into_owned()))
    }
}

pub fn passwd_user_name() -> Option<String> {
    // SAFETY: as in `passwd_home`.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr((*pw).pw_name);
        Some(name.to_string_lossy().into_owned())
    }
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}
