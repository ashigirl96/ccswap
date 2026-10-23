//! Anthropic's OAuth endpoints (upstream `oauth.py`) and the credential
//! helpers that go with them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::endpoints::Endpoints;
use crate::http_client::{self, classify_transport, parse_retry_after};
use crate::usage::{PyError, format_reset, py_number, truthy};

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
    pub detail: String,
}

impl UsageFailure {
    fn kind(kind: &str, detail: impl std::fmt::Display) -> Self {
        UsageFailure {
            kind: kind.into(),
            retry_after_s: None,
            status: None,
            detail: detail.to_string(),
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

/// Who a refreshed token belongs to, when the token endpoint says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenAccount {
    pub uuid: String,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

/// A refresh-token grant attempt. `error` is `None` on success,
/// `invalid_grant` / `no_refresh_token` for a dead lineage, `invalid_client`
/// for a rejected client, `transient` for anything retryable, or one of the
/// consume gate's own refusals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshOutcome {
    pub credentials: Option<String>,
    pub error: Option<String>,
    pub token_account: Option<TokenAccount>,
    /// The generation actually POSTed, which may differ from the caller's
    /// snapshot: strikes bind to it.
    pub consumed_fp: Option<String>,
    /// On a demoted outcome (`transient` with credentials): whether the
    /// successor reached the unclaimed stash.
    pub stashed: bool,
}

impl RefreshOutcome {
    pub fn failed(error: &str) -> Self {
        RefreshOutcome {
            error: Some(error.into()),
            ..Default::default()
        }
    }
}

pub const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// `GET /api/oauth/usage`, normalised.
    pub fn fetch_usage(&self, access_token: &str) -> Result<Option<Value>, UsageFailure> {
        let response = http_client::agent(USAGE_TIMEOUT)
            .get(&self.endpoints.usage_url())
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("anthropic-beta", OAUTH_BETA_HEADER)
            .header("User-Agent", USER_AGENT)
            .call();
        let mut response = response.map_err(|e| UsageFailure::kind(classify_transport(&e), e))?;
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
                detail: format!("HTTP Error {status}"),
            });
        }
        let bytes = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| UsageFailure::kind(classify_transport(&e), e))?;
        let text =
            String::from_utf8(bytes).map_err(|e| UsageFailure::kind("UnicodeDecodeError", e))?;
        let data: Value =
            serde_json::from_str(&text).map_err(|e| UsageFailure::kind("bad-response", e))?;
        build_usage_result(&data).map_err(|e| UsageFailure::kind(e, e))
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
            None => RefreshOutcome::failed("transient"),
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

        let context = format!("for account {num}");
        let failure = match self.fetch_usage(&access_token) {
            Ok(usage) => return UsageOutcome::ok(usage),
            Err(f) => f,
        };
        if failure.status != Some(401) || is_active || !has_refresh {
            log_usage_failure(&context, &failure, failure.retry_after_s);
            return UsageOutcome::failed(failure.kind, failure.retry_after_s);
        }
        let outcome = refresh(credentials);
        let Some(fresh) = outcome.credentials.clone().filter(|c| !c.is_empty()) else {
            log_usage_failure(&context, &failure, None);
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
            Err(f) => {
                log_usage_failure(&format!("{context} after refresh"), &f, f.retry_after_s);
                UsageOutcome::failed(f.kind, f.retry_after_s)
            }
        }
    }

    /// Only a 4xx whose body names `invalid_grant` / `invalid_client` is
    /// permanent, since a wrongly permanent verdict quarantines a live token.
    pub fn try_refresh_oauth_credentials(
        &self,
        credentials: &str,
        timeout: Duration,
    ) -> RefreshOutcome {
        let Ok(Value::Object(mut data)) = serde_json::from_str::<Value>(credentials) else {
            return RefreshOutcome::failed("transient");
        };
        let refresh_token = match data.get("claudeAiOauth") {
            Some(Value::Object(oauth)) if oauth.get("refreshToken").is_some_and(truthy) => {
                oauth["refreshToken"].clone()
            }
            _ => return RefreshOutcome::failed("no_refresh_token"),
        };
        let body = crate::pyjson::to_string_compact(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": OAUTH_CLIENT_ID,
        }));
        let response = http_client::agent(timeout)
            .post(&self.endpoints.oauth_token_url)
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .send(body.as_bytes());
        let Ok(mut response) = response else {
            crate::log::debug("OAuth refresh failed: network error");
            return RefreshOutcome::failed("transient");
        };
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        if status >= 400 {
            let preview: String = text.chars().take(500).collect();
            crate::log::debug(format!(
                "OAuth refresh failed: HTTP Error {status}, body: {preview}"
            ));
            if matches!(status, 400 | 401 | 403)
                && let Ok(Value::Object(err)) = serde_json::from_str::<Value>(&text)
                && let Some(kind @ ("invalid_grant" | "invalid_client")) =
                    err.get("error").and_then(Value::as_str)
            {
                return RefreshOutcome::failed(kind);
            }
            return RefreshOutcome::failed("transient");
        }
        let Some((credentials, token_account)) = apply_token_response(&mut data, &text) else {
            crate::log::debug("OAuth refresh failed: malformed token response");
            return RefreshOutcome::failed("transient");
        };
        RefreshOutcome {
            credentials: Some(credentials),
            token_account,
            ..Default::default()
        }
    }

    /// `GET /api/oauth/profile`. `None` means unresolvable (offline, 401,
    /// schema drift), never "wrong". Must not be called under a lock.
    pub fn fetch_profile(&self, access_token: &str) -> Option<Profile> {
        let fetched = http_client::agent(PROFILE_TIMEOUT)
            .get(&self.endpoints.profile_url())
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| e.to_string())
            .and_then(|mut response| match response.status().as_u16() {
                200 => response
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| e.to_string()),
                401 => Err(String::new()),
                status => Err(format!("HTTP Error {status}")),
            })
            .and_then(|body| serde_json::from_str::<Value>(&body).map_err(|e| e.to_string()));
        match fetched {
            Ok(data) => parse_profile(&data),
            Err(e) if e.is_empty() => {
                crate::log::warning(
                    "OAuth profile returned 401 while resolving credential ownership; proceeding without identity (pre-fix behavior).",
                );
                None
            }
            Err(e) => {
                crate::log::debug(format!("OAuth profile fetch failed: {e}"));
                None
            }
        }
    }
}

/// One WARNING with the cause; `context` never carries the email because
/// users paste these lines into public issues.
fn log_usage_failure(context: &str, failure: &UsageFailure, retry_after_s: Option<f64>) {
    let kind = failure.kind.as_str();
    let mut cause = match retry_after_s {
        Some(secs) => format!("{kind}, retry-after {secs:.0}s"),
        None => kind.to_string(),
    };
    if kind == "http-429" {
        cause.push_str(" (usage-endpoint budget reached; backing off)");
    }
    crate::log::warning(format!("Usage fetch failed {context}: {cause}"));
    crate::log::debug(format!(
        "Usage fetch failure detail {context}: {}",
        failure.detail
    ));
}

fn parse_profile(data: &Value) -> Option<Profile> {
    let Some(account) = data.get("account").and_then(Value::as_object) else {
        crate::log::debug("OAuth profile response missing account object");
        return None;
    };
    let Some(uuid) = account
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        crate::log::debug("OAuth profile response missing account.uuid");
        return None;
    };
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

/// Rotates `data`'s `claudeAiOauth` with a token-endpoint response;
/// `None` when the response is not the expected shape.
fn apply_token_response(
    data: &mut Map<String, Value>,
    text: &str,
) -> Option<(String, Option<TokenAccount>)> {
    let Value::Object(resp) = serde_json::from_str::<Value>(text).ok()? else {
        return None;
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let expires_at = match resp.get("expires_in")? {
        Value::Bool(b) => Value::from(now_ms + i64::from(*b) * 1000),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Value::from(now_ms + i * 1000),
            None => Value::from(now_ms as f64 + n.as_f64()? * 1000.0),
        },
        _ => return None,
    };
    let access_token = resp.get("access_token")?.clone();
    let scopes = match resp.get("scope") {
        Some(scope) if truthy(scope) => Some(Value::from(
            scope
                .as_str()?
                .split_whitespace()
                .map(Value::from)
                .collect::<Vec<_>>(),
        )),
        _ => None,
    };
    let Some(Value::Object(oauth)) = data.get_mut("claudeAiOauth") else {
        return None;
    };
    oauth.insert("accessToken".into(), access_token);
    oauth.insert("expiresAt".into(), expires_at);
    if let Some(rt) = resp.get("refresh_token").filter(|v| truthy(v)) {
        oauth.insert("refreshToken".into(), rt.clone());
    }
    if let Some(scopes) = scopes {
        oauth.insert("scopes".into(), scopes);
    }
    let credentials = crate::pyjson::to_string_compact(&Value::Object(data.clone()));
    Some((credentials, parse_token_account(&resp)))
}

/// The optional identity a refresh response carries; usable only with a
/// non-blank `account.uuid`.
fn parse_token_account(resp: &Map<String, Value>) -> Option<TokenAccount> {
    let account = resp.get("account")?.as_object()?;
    let uuid = account.get("uuid")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    Some(TokenAccount {
        uuid: uuid.to_string(),
        email: account
            .get("email_address")
            .and_then(Value::as_str)
            .map(str::to_string),
        organization_uuid: resp
            .get("organization")
            .and_then(Value::as_object)
            .and_then(|o| o.get("uuid"))
            .and_then(Value::as_str)
            .map(str::to_string),
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
    let (countdown, clock) = format_reset(&Value::String(iso)).ok()?;
    let state = if is_oauth_token_expired(expires_at) {
        "expired"
    } else {
        "fresh"
    };
    Some(format!(
        "oauth: {state}, refresh token {refresh}, expires {clock} in {countdown}"
    ))
}

fn py_get<'a>(v: &'a Value, key: &str) -> Result<Option<&'a Value>, PyError> {
    match v {
        Value::Object(o) => Ok(o.get(key)),
        _ => Err("AttributeError"),
    }
}

fn py_index<'a>(v: &'a Value, key: &str) -> Result<&'a Value, PyError> {
    match v {
        Value::Object(o) => o.get(key).ok_or("KeyError"),
        _ => Err("TypeError"),
    }
}

fn add_reset(entry: &mut Map<String, Value>, resets_at: &Value) -> Result<(), PyError> {
    entry.insert("resets_at".into(), resets_at.clone());
    let (countdown, clock) = format_reset(resets_at)?;
    entry.insert("countdown".into(), countdown.into());
    entry.insert("clock".into(), clock.into());
    Ok(())
}

/// Normalises a usage API body. `Ok(None)` is a successful fetch that
/// carried no window; `Err` names the Python exception upstream would hit.
pub fn build_usage_result(data: &Value) -> Result<Option<Value>, PyError> {
    crate::log::debug(format!(
        "Usage API response: {}",
        crate::pyjson::to_string_pretty(data)
    ));
    let mut result = Map::new();
    for key in ["five_hour", "seven_day"] {
        let Some(window) = py_get(data, key)?.filter(|w| truthy(w)) else {
            continue;
        };
        let mut entry = Map::new();
        entry.insert("pct".into(), py_index(window, "utilization")?.clone());
        if let Some(resets_at) = py_get(window, "resets_at")?.filter(|v| truthy(v)) {
            add_reset(&mut entry, resets_at)?;
        }
        result.insert(key.into(), Value::Object(entry));
    }
    if let Some(eu) = py_get(data, "extra_usage")?.filter(|v| truthy(v))
        && py_get(eu, "is_enabled")?.is_some_and(truthy)
    {
        match spend_entry(eu) {
            Ok(Some(spend)) => {
                result.insert("spend".into(), Value::Object(spend));
            }
            Ok(None) => {}
            Err(e) => crate::log::debug(format!("extra_usage parse failed: {e}")),
        }
    }
    if let Some(Value::Array(limits)) = py_get(data, "limits")? {
        let mut scoped = Vec::new();
        for lim in limits {
            let Value::Object(lim) = lim else { continue };
            let name = lim
                .get("scope")
                .and_then(Value::as_object)
                .and_then(|s| s.get("model"))
                .and_then(Value::as_object)
                .and_then(|m| m.get("display_name"))
                .filter(|n| truthy(n));
            let (Some(name), Some(pct)) = (name, lim.get("percent").and_then(py_number)) else {
                continue;
            };
            let mut entry = Map::new();
            entry.insert("name".into(), name.clone());
            entry.insert("pct".into(), json!(pct));
            if let Some(resets_at) = lim.get("resets_at").filter(|v| truthy(v)) {
                add_reset(&mut entry, resets_at)?;
            }
            scoped.push(Value::Object(entry));
        }
        if !scoped.is_empty() {
            result.insert("scoped".into(), Value::Array(scoped));
        }
    }
    Ok((!result.is_empty()).then_some(Value::Object(result)))
}

/// `float(v)` for the values JSON can carry; `None` where Python raises.
fn py_float(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            match t.as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => t.replace('_', "").parse().ok(),
            }
        }
        other => py_number(other),
    }
}

/// `Err` carries the exception upstream's parse would log.
fn spend_entry(eu: &Value) -> Result<Option<Map<String, Value>>, String> {
    let Some(eu) = eu.as_object() else {
        return Ok(None);
    };
    let field = |k: &str| eu.get(k).filter(|v| !v.is_null());
    let (Some(used), Some(limit), Some(pct)) = (
        field("used_credits"),
        field("monthly_limit"),
        field("utilization"),
    ) else {
        return Ok(None);
    };
    let mut entry = Map::new();
    entry.insert("used".into(), json!(float_or_error(used)? / 100.0));
    entry.insert("limit".into(), json!(float_or_error(limit)? / 100.0));
    entry.insert("pct".into(), json!(float_or_error(pct)?));
    entry.insert(
        "currency".into(),
        eu.get("currency").cloned().unwrap_or_else(|| "USD".into()),
    );
    if let Some(resets_at) = eu.get("resets_at").filter(|v| truthy(v)) {
        add_reset(&mut entry, resets_at).map_err(|e| format!("{e}()"))?;
    }
    Ok(Some(entry))
}

fn float_or_error(v: &Value) -> Result<f64, String> {
    py_float(v).ok_or_else(|| match v {
        Value::String(s) => format!("ValueError(\"could not convert string to float: '{s}'\")"),
        other => format!(
            "TypeError(\"float() argument must be a string or a real number, not '{}'\")",
            crate::pyjson::py_type_name(other)
        ),
    })
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_oauth.py (TestFetchOauthProfile,
    //! TestCredentialFingerprint, TestBuildTokenStatus, TestFetchUsage
    //! normalisation).
    use super::*;
    use serde_json::json;

    fn iso_in(secs: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
    }

    #[test]
    fn success_normalises_windows_and_keeps_pct_type() {
        let r = build_usage_result(&json!({
            "five_hour": {"utilization": 25, "resets_at": iso_in(3600 + 30)},
            "seven_day": {"utilization": 60.0, "resets_at": null},
        }))
        .unwrap()
        .unwrap();
        assert_eq!(r["five_hour"]["pct"], json!(25));
        assert_eq!(r["five_hour"]["countdown"], "1h 0m");
        assert_eq!(r["seven_day"], json!({"pct": 60.0}));
        let keys: Vec<_> = r.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["five_hour", "seven_day"]);
    }

    #[test]
    fn empty_body_is_a_successful_fetch_without_usage() {
        assert_eq!(build_usage_result(&json!({})), Ok(None));
    }

    #[test]
    fn malformed_bodies_fail_with_python_exception_names() {
        assert_eq!(build_usage_result(&json!([1])), Err("AttributeError"));
        assert_eq!(
            build_usage_result(&json!({"five_hour": {"resets_at": "x"}})),
            Err("KeyError")
        );
        assert_eq!(
            build_usage_result(&json!({"five_hour": [1]})),
            Err("TypeError")
        );
        assert_eq!(
            build_usage_result(&json!({"five_hour": {"utilization": 1, "resets_at": "nope"}})),
            Err("ValueError")
        );
    }

    #[test]
    fn extra_usage_complete_adds_spend() {
        let r = build_usage_result(&json!({
            "five_hour": {"utilization": 10.0},
            "extra_usage": {"is_enabled": true, "used_credits": 1250, "monthly_limit": 30000,
                            "utilization": 4, "currency": "EUR"},
        }))
        .unwrap()
        .unwrap();
        assert_eq!(
            r["spend"],
            json!({"used": 12.5, "limit": 300.0, "pct": 4.0, "currency": "EUR"})
        );
    }

    #[test]
    fn extra_usage_partial_unlimited_or_disabled_keeps_other_rows() {
        for eu in [
            json!({"is_enabled": true, "used_credits": 5, "monthly_limit": null, "utilization": 1}),
            json!({"is_enabled": true, "used_credits": null, "monthly_limit": 5, "utilization": 1}),
            json!({"is_enabled": false, "used_credits": 5, "monthly_limit": 5, "utilization": 1}),
        ] {
            let r =
                build_usage_result(&json!({"five_hour": {"utilization": 1}, "extra_usage": eu}))
                    .unwrap()
                    .unwrap();
            assert!(r.get("spend").is_none());
            assert_eq!(r["five_hour"]["pct"], 1);
        }
    }

    #[test]
    fn scoped_per_model_limits() {
        let r = build_usage_result(&json!({
            "limits": [
                {"scope": {"model": {"display_name": "Fable"}}, "percent": 100},
                {"scope": {"model": {"display_name": ""}}, "percent": 5},
                {"scope": null, "percent": 5},
                {"scope": {"model": {"display_name": "Opus"}}, "percent": "7"},
            ]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(r, json!({"scoped": [{"name": "Fable", "pct": 100.0}]}));
        assert_eq!(
            build_usage_result(&json!({"five_hour": {"utilization": 1}}))
                .unwrap()
                .unwrap()
                .get("scoped"),
            None
        );
    }

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
