//! Finding the `codex` binary when `PATH` is launchd's or Finder's minimal one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::env::Environment;
use crate::path_search::which;

/// Where Codex installs put `codex`, searched after `PATH`.
const FALLBACK_DIRS: [&str; 9] = [
    "~/.local/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "~/.bun/bin",
    "~/.npm-global/bin",
    "~/.volta/bin",
    "~/.local/share/mise/shims",
    "~/.asdf/shims",
    "~/.cargo/bin",
];

/// The `codex` to run and the `PATH` it runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexProgram {
    path: PathBuf,
    search_path: OsString,
}

impl CodexProgram {
    /// `PATH` first, then the usual install dirs; plain `codex` when none has it.
    pub fn resolve(env: &Environment) -> Self {
        let home = env.home();
        let mut dirs: Vec<PathBuf> = env
            .get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        let fallbacks = FALLBACK_DIRS
            .iter()
            .map(|d| match d.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(d),
            })
            .chain(nvm_bin_dirs(&home));
        for dir in fallbacks {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        // The npm-installed `codex` is a node script, so its child needs node on PATH too.
        let search_path = std::env::join_paths(&dirs).unwrap_or_default();
        let lookup = Environment::from_pairs([("PATH", search_path.clone())]);
        CodexProgram {
            path: which(&lookup, "codex").unwrap_or_else(|| PathBuf::from("codex")),
            search_path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.path);
        command.env("PATH", &self.search_path);
        command
    }
}

/// Newest node first, by the version directory's name.
fn nvm_bin_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| Some(e.ok()?.path().join("bin")))
        .collect();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn env(home: &Path, path: &str) -> Environment {
        Environment::from_pairs([("HOME", home.as_os_str()), ("PATH", path.as_ref())])
    }

    #[test]
    fn prefers_codex_on_path() {
        let tmp = tempfile::tempdir().unwrap();
        let on_path = tmp.path().join("bin/codex");
        executable(&on_path);
        executable(&tmp.path().join(".bun/bin/codex"));
        let bin = tmp.path().join("bin");
        let program = CodexProgram::resolve(&env(tmp.path(), bin.to_str().unwrap()));
        assert_eq!(program.path(), on_path);
    }

    #[test]
    fn finds_codex_in_an_install_dir_missing_from_a_launchd_path() {
        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join(".local/bin/codex");
        executable(&installed);
        std::fs::create_dir_all(tmp.path().join(".nvm/versions/node/v22.1.0/bin")).unwrap();
        let program = CodexProgram::resolve(&env(tmp.path(), "/usr/bin:/bin"));
        assert_eq!(program.path(), installed);
        let child_path = program.search_path.to_string_lossy().into_owned();
        assert!(child_path.starts_with("/usr/bin:/bin:"));
        assert!(child_path.contains(".nvm/versions/node/v22.1.0/bin"));
    }
}
