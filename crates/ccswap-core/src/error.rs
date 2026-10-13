use std::fmt;

/// Mirrors upstream's exception classes; the variant name is the `type` of
/// the `--json` error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    ClaudeSwitchError,
    CredentialError,
    CredentialReadError,
    CredentialWriteError,
    ConfigError,
    SwitchError,
    SessionError,
    LockError,
    ClaudeCodeLockTimeout,
    AccountNotFoundError,
    ValidationError,
    TransferError,
    MigrationError,
    MigrationIncomplete,
}

impl ErrorKind {
    pub fn type_name(self) -> String {
        format!("{self:?}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ConfigError, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
