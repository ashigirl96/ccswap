use std::path::PathBuf;

use crate::env::Environment;

pub const SECURITY_ENV: &str = "CCSWAP_SECURITY_PATH";
pub const API_BASE_URL_ENV: &str = "CCSWAP_API_BASE_URL";
pub const OAUTH_TOKEN_URL_ENV: &str = "CCSWAP_OAUTH_TOKEN_URL";
pub const UPDATE_CHECK_URL_ENV: &str = "CCSWAP_UPDATE_CHECK_URL";
pub const CODEX_API_BASE_URL_ENV: &str = "CCSWAP_CODEX_API_BASE_URL";
pub const OPENAI_AUTHORIZE_URL_ENV: &str = "CCSWAP_OPENAI_AUTHORIZE_URL";
pub const OPENAI_TOKEN_URL_ENV: &str = "CCSWAP_OPENAI_TOKEN_URL";
pub const OPENAI_JWKS_URL_ENV: &str = "CCSWAP_OPENAI_JWKS_URL";
pub const OPENAI_RESPONSES_URL_ENV: &str = "CCSWAP_OPENAI_RESPONSES_URL";

pub const DEFAULT_SECURITY: &str = "/usr/bin/security";
pub const DEFAULT_API_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const DEFAULT_UPDATE_CHECK_URL: &str =
    "https://api.github.com/repos/ashigirl96/ccswap/releases/latest";
pub const DEFAULT_CODEX_API_BASE_URL: &str = "https://chatgpt.com";

/// External programs and HTTP endpoints, overridable so tests can point them
/// at doubles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub security: PathBuf,
    pub api_base_url: String,
    pub oauth_token_url: String,
    pub update_check_url: String,
    pub codex_api_base_url: String,
    pub openai_authorize_url: String,
    pub openai_token_url: String,
    pub openai_jwks_url: String,
    pub openai_responses_url: String,
}

impl Endpoints {
    pub fn new(env: &Environment) -> Self {
        let pick =
            |name: &str, default: &str| env.non_empty(name).unwrap_or_else(|| default.into());
        Endpoints {
            security: PathBuf::from(pick(SECURITY_ENV, DEFAULT_SECURITY)),
            api_base_url: pick(API_BASE_URL_ENV, DEFAULT_API_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            oauth_token_url: pick(OAUTH_TOKEN_URL_ENV, DEFAULT_OAUTH_TOKEN_URL),
            update_check_url: pick(UPDATE_CHECK_URL_ENV, DEFAULT_UPDATE_CHECK_URL),
            codex_api_base_url: pick(CODEX_API_BASE_URL_ENV, DEFAULT_CODEX_API_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            openai_authorize_url: pick(
                OPENAI_AUTHORIZE_URL_ENV,
                "https://auth.openai.com/api/accounts/authorize",
            ),
            openai_token_url: pick(
                OPENAI_TOKEN_URL_ENV,
                "https://auth.openai.com/api/accounts/oauth/token",
            ),
            openai_responses_url: pick(
                OPENAI_RESPONSES_URL_ENV,
                "https://api.openai.com/v1/responses",
            ),
            openai_jwks_url: pick(
                OPENAI_JWKS_URL_ENV,
                "https://auth.openai.com/.well-known/jwks.json",
            ),
        }
    }

    pub fn codex_usage_url(&self) -> String {
        format!("{}/backend-api/wham/usage", self.codex_api_base_url)
    }

    pub fn usage_url(&self) -> String {
        format!("{}/api/oauth/usage", self.api_base_url)
    }

    pub fn profile_url(&self) -> String {
        format!("{}/api/oauth/profile", self.api_base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_upstreams_endpoints() {
        let e = Endpoints::new(&Environment::default());
        assert_eq!(e.security, PathBuf::from("/usr/bin/security"));
        assert_eq!(e.usage_url(), "https://api.anthropic.com/api/oauth/usage");
        assert_eq!(
            e.profile_url(),
            "https://api.anthropic.com/api/oauth/profile"
        );
        assert_eq!(
            e.oauth_token_url,
            "https://platform.claude.com/v1/oauth/token"
        );
        assert_eq!(
            e.codex_usage_url(),
            "https://chatgpt.com/backend-api/wham/usage"
        );
    }

    #[test]
    fn environment_overrides_every_endpoint() {
        let e = Endpoints::new(&Environment::from_pairs([
            (SECURITY_ENV, "/tmp/fake-security"),
            (API_BASE_URL_ENV, "http://127.0.0.1:9/"),
            (OAUTH_TOKEN_URL_ENV, "http://127.0.0.1:9/token"),
            (UPDATE_CHECK_URL_ENV, "http://127.0.0.1:9/latest"),
            (CODEX_API_BASE_URL_ENV, "http://127.0.0.1:9/"),
        ]));
        assert_eq!(
            e.codex_usage_url(),
            "http://127.0.0.1:9/backend-api/wham/usage"
        );
        assert_eq!(e.security, PathBuf::from("/tmp/fake-security"));
        assert_eq!(e.usage_url(), "http://127.0.0.1:9/api/oauth/usage");
        assert_eq!(e.oauth_token_url, "http://127.0.0.1:9/token");
        assert_eq!(e.update_check_url, "http://127.0.0.1:9/latest");
    }

    #[test]
    fn empty_override_falls_back_to_default() {
        let e = Endpoints::new(&Environment::from_pairs([(SECURITY_ENV, "")]));
        assert_eq!(e.security, PathBuf::from("/usr/bin/security"));
    }
}
