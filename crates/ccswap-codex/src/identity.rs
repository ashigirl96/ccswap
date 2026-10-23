use base64::Engine;
use ccswap_core::error::{Error, ErrorKind, Result};
use serde_json::Value;

const AUTH_CLAIMS: &str = "https://api.openai.com/auth";
const PROFILE_CLAIMS: &str = "https://api.openai.com/profile";

/// Who an `auth.json` belongs to. JWT signatures are not verified: the file is the user's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexIdentity {
    /// Trimmed and lowercased; the account's identity.
    pub account_id: String,
    /// Lowercased; empty when the id_token carries none.
    pub email: String,
    pub plan: String,
}

/// Reads the identity of `auth.json` bytes; fails for API-key-only logins.
pub fn read_identity(bytes: &[u8]) -> Result<CodexIdentity> {
    let auth: Value = serde_json::from_slice(bytes).map_err(|e| {
        Error::new(
            ErrorKind::CredentialReadError,
            format!("Codex auth.json is not valid JSON: {e}"),
        )
    })?;
    let Some(tokens) = auth.get("tokens").filter(|t| t.is_object()) else {
        return Err(Error::new(
            ErrorKind::CredentialError,
            "Codex auth.json has no ChatGPT tokens (an API key login?). Only ChatGPT logins can be managed.",
        ));
    };
    let claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims)
        .unwrap_or(Value::Null);
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
    };
    let account_id = text(tokens.get("account_id"))
        .or_else(|| {
            text(
                claims
                    .get(AUTH_CLAIMS)
                    .and_then(|a| a.get("chatgpt_account_id")),
            )
        })
        .or_else(|| text(claims.get("chatgpt_account_id")))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::CredentialError,
                "Codex auth.json has no ChatGPT account id",
            )
        })?;
    let email = text(claims.get("email"))
        .or_else(|| text(claims.get(PROFILE_CLAIMS).and_then(|p| p.get("email"))))
        .unwrap_or_default();
    let plan = claims
        .get(AUTH_CLAIMS)
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(CodexIdentity {
        account_id,
        email,
        plan,
    })
}

/// The payload of an unverified JWT.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
