use crate::secret_store::{ActiveCredentials, extract_access_token, looks_like_api_key};

/// Usage states that stand in for a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sentinel {
    NoCredentials,
    TokenExpired,
    ApiKey,
    KeychainUnavailable,
    ReloginRequired,
    ForeignCredential,
}

impl Sentinel {
    /// The `usageStatus` value in `--json` output.
    pub fn json_status(self) -> &'static str {
        match self {
            Sentinel::NoCredentials => "no_credentials",
            Sentinel::TokenExpired => "token_expired",
            Sentinel::ApiKey => "api_key",
            Sentinel::KeychainUnavailable => "keychain_unavailable",
            Sentinel::ReloginRequired => "relogin_required",
            Sentinel::ForeignCredential => "foreign_credential",
        }
    }

    /// The one-line note in human output.
    pub fn note(self) -> &'static str {
        match self {
            Sentinel::NoCredentials => "no credentials",
            Sentinel::TokenExpired => {
                "token expired — refresh deferred this pass; retries automatically"
            }
            Sentinel::ApiKey => "API key (no quota)",
            Sentinel::KeychainUnavailable => "keychain unavailable — locked or in use; try again",
            Sentinel::ReloginRequired => {
                "re-login needed — refresh token dead; log in with Claude Code, then run: ccswap add"
            }
            Sentinel::ForeignCredential => {
                "live credential belongs to another account — a switch repairs it"
            }
        }
    }
}

/// One account's usage as the status and list views show it.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageEntry {
    Sentinel(Sentinel),
    /// No measurement and no network-free verdict.
    Unavailable,
}

/// The verdict for the active account that needs no network call.
pub fn static_active_sentinel(active: &ActiveCredentials) -> Option<Sentinel> {
    let creds = active.value.as_deref().unwrap_or_default();
    if looks_like_api_key(creds) {
        return Some(Sentinel::ApiKey);
    }
    if creds.is_empty() || extract_access_token(creds).is_none_or(|t| t.is_empty()) {
        if active.keychain_unavailable || active.value.is_none() {
            return Some(Sentinel::KeychainUnavailable);
        }
        return Some(Sentinel::NoCredentials);
    }
    None
}
