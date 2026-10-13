use std::path::PathBuf;

use crate::env::Environment;

pub const BACKUP_DIRNAME: &str = ".claude-swap-backup";

/// Claude Code's and claude-swap's file locations, resolved the way Claude
/// Code resolves them.
#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
    config_dir: Option<String>,
}

impl Paths {
    pub fn new(env: &Environment) -> Self {
        Paths {
            home: env.home(),
            config_dir: env.non_empty("CLAUDE_CONFIG_DIR"),
        }
    }

    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    /// The raw `CLAUDE_CONFIG_DIR` value, unnormalised (Keychain names hash it as-is).
    pub fn config_dir_env(&self) -> Option<&str> {
        self.config_dir.as_deref()
    }

    pub fn claude_config_home(&self) -> PathBuf {
        match &self.config_dir {
            Some(dir) => PathBuf::from(dir),
            None => self.default_claude_config_home(),
        }
    }

    pub fn default_claude_config_home(&self) -> PathBuf {
        self.home.join(".claude")
    }

    /// `<config_home>/.config.json` when it exists (legacy), else
    /// `(CLAUDE_CONFIG_DIR or $HOME)/.claude.json`.
    pub fn global_config_path(&self) -> PathBuf {
        let legacy = self.claude_config_home().join(".config.json");
        if legacy.exists() {
            return legacy;
        }
        match &self.config_dir {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => self.home.join(".claude.json"),
        }
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.claude_config_home().join(".credentials.json")
    }

    pub fn backup_root(&self) -> PathBuf {
        self.home.join(BACKUP_DIRNAME)
    }

    pub fn sequence_file(&self) -> PathBuf {
        self.backup_root().join("sequence.json")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.backup_root().join("settings.json")
    }

    /// Whether the active config home is the default profile's, by resolved path.
    pub fn active_profile_is_default(&self) -> bool {
        match (
            self.claude_config_home().canonicalize(),
            self.default_claude_config_home().canonicalize(),
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(pairs: &[(&str, &str)]) -> Paths {
        Paths::new(&Environment::from_pairs(pairs.iter().copied()))
    }

    #[test]
    fn default_profile_keeps_claude_json_in_home() {
        let p = paths(&[("HOME", "/h")]);
        assert_eq!(p.claude_config_home(), PathBuf::from("/h/.claude"));
        assert_eq!(p.global_config_path(), PathBuf::from("/h/.claude.json"));
        assert_eq!(
            p.credentials_path(),
            PathBuf::from("/h/.claude/.credentials.json")
        );
        assert_eq!(p.backup_root(), PathBuf::from("/h/.claude-swap-backup"));
    }

    #[test]
    fn claude_config_dir_moves_config_home_and_global_config() {
        let p = paths(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "/c")]);
        assert_eq!(p.claude_config_home(), PathBuf::from("/c"));
        assert_eq!(p.global_config_path(), PathBuf::from("/c/.claude.json"));
        assert_eq!(p.backup_root(), PathBuf::from("/h/.claude-swap-backup"));
    }

    #[test]
    fn empty_claude_config_dir_counts_as_unset() {
        let p = paths(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "")]);
        assert_eq!(p.claude_config_home(), PathBuf::from("/h/.claude"));
    }

    #[test]
    fn legacy_config_json_takes_precedence_when_present() {
        let dir = std::env::temp_dir().join(format!("ccswap-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(dir.join(".claude/.config.json"), "{}").unwrap();
        let p = paths(&[("HOME", dir.to_str().unwrap())]);
        assert_eq!(p.global_config_path(), dir.join(".claude/.config.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
