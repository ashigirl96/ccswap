//! Anthropic's OAuth endpoints (upstream `oauth.py`) and the credential
//! helpers that go with them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::endpoints::Endpoints;

pub const USER_AGENT: &str = "claude-swap/1.0";
pub const OAUTH_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whose token this is, as the profile endpoint answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub uuid: String,
    pub email: Option<String>,
    /// `None` when the response carried no organization block at all.
    pub organization_uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnthropicClient {
    endpoints: Endpoints,
}

impl AnthropicClient {
    pub fn new(endpoints: Endpoints) -> Self {
        AnthropicClient { endpoints }
    }

    /// `GET /api/oauth/profile`. `None` means unresolvable (offline, 401,
    /// schema drift), never "wrong". Must not be called under a lock.
    pub fn fetch_profile(&self, access_token: &str) -> Option<Profile> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(PROFILE_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(&self.endpoints.profile_url())
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .call()
            .ok()?;
        if response.status().as_u16() != 200 {
            return None;
        }
        let body = response.body_mut().read_to_string().ok()?;
        parse_profile(&serde_json::from_str(&body).ok()?)
    }
}

fn parse_profile(data: &Value) -> Option<Profile> {
    let account = data.get("account")?.as_object()?;
    let uuid = account.get("uuid")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let organization_uuid = data
        .get("organization")
        .and_then(Value::as_object)
        .and_then(|o| o.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(Profile {
        uuid: uuid.to_string(),
        email: account
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        organization_uuid,
    })
}

/// `claudeAiOauth`, when the credential is OAuth JSON.
pub fn extract_oauth_data(credentials: &str) -> Option<Map<String, Value>> {
    let value: Value = serde_json::from_str(credentials).ok()?;
    value.get("claudeAiOauth")?.as_object().cloned()
}

/// Lineage identity: the refresh token's hash when there is one (so an
/// access-token rotation compares equal), else the whole credential's.
pub fn credential_fingerprint(credentials: &str) -> Option<String> {
    if credentials.is_empty() {
        return None;
    }
    let refresh = extract_oauth_data(credentials)
        .and_then(|d| {
            d.get("refreshToken")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|t| !t.is_empty());
    Some(match refresh {
        Some(token) => format!("sha256:{}", sha256_hex(token.as_bytes())),
        None => format!("sha256-full:{}", sha256_hex(credentials.as_bytes())),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Expired or within five minutes of it; a non-numeric `expiresAt` is not.
pub fn is_oauth_token_expired(expires_at: Option<&Value>) -> bool {
    let Some(expires_at) = expires_at.and_then(Value::as_f64) else {
        return false;
    };
    if expires_at.is_nan() {
        return false;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    now_ms + OAUTH_EXPIRY_BUFFER_MS >= expires_at as i64
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_oauth.py (TestFetchOauthProfile,
    //! TestCredentialFingerprint).
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_needs_a_non_blank_account_uuid() {
        assert_eq!(parse_profile(&json!({"account": {"email": "a@x"}})), None);
        assert_eq!(parse_profile(&json!({"account": {"uuid": "  "}})), None);
        assert_eq!(parse_profile(&json!({"account": "nope"})), None);
        assert_eq!(parse_profile(&json!([1])), None);
    }

    #[test]
    fn profile_fields_are_optional_and_trimmed() {
        assert_eq!(
            parse_profile(&json!({"account": {"uuid": " u1 ", "email": 3}})),
            Some(Profile {
                uuid: "u1".into(),
                email: None,
                organization_uuid: None
            })
        );
        assert_eq!(
            parse_profile(&json!({
                "account": {"uuid": "u1", "email": "a@x"},
                "organization": {"uuid": "org"}
            })),
            Some(Profile {
                uuid: "u1".into(),
                email: Some("a@x".into()),
                organization_uuid: Some("org".into())
            })
        );
    }

    #[test]
    fn fingerprint_follows_the_refresh_token() {
        let a = r#"{"claudeAiOauth": {"accessToken": "a1", "refreshToken": "rt"}}"#;
        let b = r#"{"claudeAiOauth": {"accessToken": "a2", "refreshToken": "rt"}}"#;
        assert_eq!(credential_fingerprint(a), credential_fingerprint(b));
        assert_eq!(
            credential_fingerprint(a).as_deref(),
            Some("sha256:cdffd5dd8ca8126c0482ba994814b9014cc9e973435d399f1cf1f69479e6b907")
        );
        assert_eq!(
            credential_fingerprint("sk-ant-api03-x").as_deref(),
            Some("sha256-full:02405cf3fdc2ea144c0d1ea1071e8bfeb06dc29cda9608e2f51458ba87d688a9")
        );
        assert_eq!(credential_fingerprint(""), None);
    }

    #[test]
    fn expiry_has_a_five_minute_buffer_and_ignores_non_numbers() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(is_oauth_token_expired(Some(&json!(1))));
        assert!(is_oauth_token_expired(Some(&json!(now_ms + 60_000))));
        assert!(!is_oauth_token_expired(Some(&json!(now_ms + 600_000))));
        assert!(!is_oauth_token_expired(Some(&json!("1"))));
        assert!(!is_oauth_token_expired(None));
    }
}
