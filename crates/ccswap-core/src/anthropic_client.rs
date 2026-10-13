//! Anthropic's OAuth endpoints (upstream `oauth.py`) and the credential
//! helpers that go with them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::endpoints::Endpoints;
use crate::usage::{build_usage_result, truthy};

pub const USER_AGENT: &str = "claude-swap/1.0";
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
pub const OAUTH_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
const USAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Refresh failures that retrying this pass cannot fix, so the expired
/// token must not be sent to the usage endpoint.
pub const DETERMINISTIC_REFRESH_ERRORS: [&str; 4] = [
    "store-unmirrored",
    "invalid_client",
    "consume-busy",
    "stash-unreadable",
];

/// A failed usage request, classified the way upstream persists it.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageFailure {
    /// `http-<code>`, `timeout`, `network`, `bad-response`, or the Python
    /// exception name upstream would record.
    pub kind: String,
    /// `Retry-After` in its seconds form.
    pub retry_after_s: Option<f64>,
    pub status: Option<u16>,
}

impl UsageFailure {
    fn kind(kind: &str) -> Self {
        UsageFailure {
            kind: kind.into(),
            retry_after_s: None,
            status: None,
        }
    }
}

/// One usage fetch attempt: `usage` may be `None` on success when the
/// response carried no window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageOutcome {
    pub usage: Option<Value>,
    pub error: Option<String>,
    pub retry_after_s: Option<f64>,
    pub struck_fp: Option<String>,
}

impl UsageOutcome {
    fn ok(usage: Option<Value>) -> Self {
        UsageOutcome {
            usage,
            ..Default::default()
        }
    }

    fn failed(kind: impl Into<String>, retry_after_s: Option<f64>) -> Self {
        UsageOutcome {
            error: Some(kind.into()),
            retry_after_s,
            ..Default::default()
        }
    }
}

/// A refresh-token grant attempt as a refresh gate reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshOutcome {
    pub credentials: Option<String>,
    pub error: Option<String>,
    pub consumed_fp: Option<String>,
}

/// Refreshes an inactive slot's credential: `(slot, email, snapshot)`.
pub type RefreshVia<'a> = &'a (dyn Fn(&str, &str, &str) -> RefreshOutcome + Sync);

fn is_dead(error: Option<&str>) -> bool {
    matches!(error, Some("invalid_grant" | "no_refresh_token"))
}

fn is_deterministic(error: Option<&str>) -> bool {
    error.is_some_and(|e| DETERMINISTIC_REFRESH_ERRORS.contains(&e))
}

fn access_token_of(credentials: &str) -> Option<String> {
    extract_oauth_data(credentials)?
        .get("accessToken")?
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

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

    /// Verifies TLS against the macOS trust store, as upstream's
    /// `truststore` does, so locally trusted roots apply.
    fn agent(timeout: Duration) -> ureq::Agent {
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .tls_config(tls)
            .build()
            .into()
    }

    /// `GET /api/oauth/usage`, normalised.
    pub fn fetch_usage(&self, access_token: &str) -> Result<Option<Value>, UsageFailure> {
        let response = Self::agent(USAGE_TIMEOUT)
            .get(&self.endpoints.usage_url())
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("anthropic-beta", OAUTH_BETA_HEADER)
            .header("User-Agent", USER_AGENT)
            .call();
        let mut response = response.map_err(|e| classify_transport(&e))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after_s = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            return Err(UsageFailure {
                kind: format!("http-{status}"),
                retry_after_s,
                status: Some(status),
            });
        }
        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| classify_transport(&e))?;
        let text =
            String::from_utf8(bytes).map_err(|_| UsageFailure::kind("UnicodeDecodeError"))?;
        let data: Value =
            serde_json::from_str(&text).map_err(|_| UsageFailure::kind("bad-response"))?;
        build_usage_result(&data).map_err(UsageFailure::kind)
    }

    /// Usage for one slot. Only inactive slots are refreshed (through
    /// `refresh_via`): Claude Code owns the active credential.
    pub fn try_fetch_usage_for_account(
        &self,
        num: &str,
        email: &str,
        credentials: &str,
        is_active: bool,
        refresh_via: Option<RefreshVia>,
    ) -> UsageOutcome {
        let oauth = extract_oauth_data(credentials);
        let (Some(oauth), Some(mut access_token)) = (oauth, access_token_of(credentials)) else {
            return UsageOutcome::failed("no-access-token", None);
        };
        let has_refresh = oauth.get("refreshToken").is_some_and(truthy);
        let refresh = |snapshot: &str| match refresh_via {
            Some(gate) => gate(num, email, snapshot),
            None => RefreshOutcome {
                error: Some("transient".into()),
                ..Default::default()
            },
        };
        let dead_outcome = |outcome: RefreshOutcome| UsageOutcome {
            struck_fp: outcome
                .consumed_fp
                .or_else(|| credential_fingerprint(credentials)),
            ..UsageOutcome::failed(outcome.error.unwrap_or_default(), None)
        };

        if !is_active && has_refresh && is_oauth_token_expired(oauth.get("expiresAt")) {
            let outcome = refresh(credentials);
            match outcome.credentials.as_deref().filter(|c| !c.is_empty()) {
                Some(fresh) => {
                    if let Some(token) = access_token_of(fresh) {
                        access_token = token;
                    }
                }
                None if is_dead(outcome.error.as_deref()) => return dead_outcome(outcome),
                None if is_deterministic(outcome.error.as_deref()) => {
                    return UsageOutcome::failed(outcome.error.unwrap_or_default(), None);
                }
                None => {}
            }
        }

        let failure = match self.fetch_usage(&access_token) {
            Ok(usage) => return UsageOutcome::ok(usage),
            Err(f) => f,
        };
        if failure.status != Some(401) || is_active || !has_refresh {
            return UsageOutcome::failed(failure.kind, failure.retry_after_s);
        }
        let outcome = refresh(credentials);
        let Some(fresh) = outcome.credentials.clone().filter(|c| !c.is_empty()) else {
            if is_dead(outcome.error.as_deref()) {
                return dead_outcome(outcome);
            }
            if is_deterministic(outcome.error.as_deref()) {
                return UsageOutcome::failed(outcome.error.unwrap_or_default(), None);
            }
            return UsageOutcome::failed("refresh-failed", None);
        };
        let Some(token) = access_token_of(&fresh) else {
            return UsageOutcome::failed("refresh-failed", None);
        };
        match self.fetch_usage(&token) {
            Ok(usage) => UsageOutcome::ok(usage),
            Err(f) => UsageOutcome::failed(f.kind, f.retry_after_s),
        }
    }

    /// `GET /api/oauth/profile`. `None` means unresolvable (offline, 401,
    /// schema drift), never "wrong". Must not be called under a lock.
    pub fn fetch_profile(&self, access_token: &str) -> Option<Profile> {
        let mut response = Self::agent(PROFILE_TIMEOUT)
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

fn classify_transport(e: &ureq::Error) -> UsageFailure {
    let timed_out = match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(io) => io.kind() == std::io::ErrorKind::TimedOut,
        _ => false,
    };
    UsageFailure::kind(if timed_out { "timeout" } else { "network" })
}

/// Python `max(0.0, float(raw.strip()))`; the HTTP-date form is ignored.
fn parse_retry_after(raw: &str) -> Option<f64> {
    let t = raw.trim().to_ascii_lowercase();
    let value: f64 = match t.as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => t.parse().ok()?,
    };
    Some(if value.is_nan() {
        value
    } else {
        value.max(0.0)
    })
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

/// `oauth: fresh, refresh token yes, expires 20:39 in 1h 30m`; `None`
/// for a non-OAuth credential.
pub fn build_token_status(credentials: &str) -> Option<String> {
    let oauth = extract_oauth_data(credentials).filter(|d| !d.is_empty())?;
    let refresh = if oauth.get("refreshToken").is_some_and(truthy) {
        "yes"
    } else {
        "no"
    };
    let expires_at = oauth.get("expiresAt").filter(|v| v.is_number());
    let Some(expires_ms) = expires_at.and_then(Value::as_f64) else {
        return Some(format!("oauth: unknown expiry, refresh token {refresh}"));
    };
    let expires = chrono::DateTime::from_timestamp_micros((expires_ms * 1000.0) as i64)?;
    let iso = expires.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string();
    let (countdown, clock) = crate::usage::format_reset(&Value::String(iso)).ok()?;
    let state = if is_oauth_token_expired(expires_at) {
        "expired"
    } else {
        "fresh"
    };
    Some(format!(
        "oauth: {state}, refresh token {refresh}, expires {clock} in {countdown}"
    ))
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_oauth.py (TestFetchOauthProfile,
    //! TestCredentialFingerprint, TestBuildTokenStatus).
    use super::*;
    use serde_json::json;

    #[test]
    fn token_status_reports_state_refresh_and_expiry() {
        let in_90m = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 90 * 60
            + 30;
        let creds = json!({"claudeAiOauth": {"accessToken": "a", "refreshToken": "r",
                                             "expiresAt": in_90m * 1000}})
        .to_string();
        let status = build_token_status(&creds).unwrap();
        assert!(
            status.starts_with("oauth: fresh, refresh token yes, expires "),
            "{status}"
        );
        assert!(status.ends_with(" in 1h 30m"), "{status}");
        let unknown = json!({"claudeAiOauth": {"accessToken": "a", "refreshToken": "r"}});
        assert_eq!(
            build_token_status(&unknown.to_string()).as_deref(),
            Some("oauth: unknown expiry, refresh token yes")
        );
        assert_eq!(build_token_status("sk-ant-api03-x"), None);
    }

    #[test]
    fn retry_after_takes_only_the_seconds_form() {
        assert_eq!(parse_retry_after(" 120 "), Some(120.0));
        assert_eq!(parse_retry_after("1e3"), Some(1000.0));
        assert_eq!(parse_retry_after("-5"), Some(0.0));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

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
