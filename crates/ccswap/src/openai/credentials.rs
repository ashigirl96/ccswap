//! Refresh only inference credentials, with rotating tokens protected by the store lock.

use ccswap_claude::openai::{InferenceLogins, token_request};
use ccswap_codex::ListedCodexAccount;
use ccswap_core::endpoints::Endpoints;
use serde_json::Value;

use super::Failure;

pub(super) fn access(
    logins: &InferenceLogins,
    account: &ListedCodexAccount,
    endpoints: &Endpoints,
    rejected: Option<&str>,
) -> Result<String, Failure> {
    let sign_in = || {
        Failure::new(
            401,
            "authentication_error",
            format!(
                "Inference login needs authorization; run ccswap claude openai login {}",
                account.number
            ),
        )
    };
    let read = || {
        logins
            .read(&account.account_id)
            .map_err(|_| Failure::upstream("Could not read the inference login"))?
            .ok_or_else(sign_in)
    };
    let usable = |login: &Value| {
        text(login, "access_token") != rejected
            && login["expires_at"]
                .as_i64()
                .is_none_or(|expires| expires > chrono::Utc::now().timestamp())
    };
    let login = read()?;
    if usable(&login) {
        return text(&login, "access_token")
            .map(str::to_owned)
            .ok_or_else(sign_in);
    }
    let _lock = logins
        .lock()
        .map_err(|_| Failure::upstream("Could not lock the inference login"))?;
    let mut login = read()?;
    if usable(&login) {
        return text(&login, "access_token")
            .map(str::to_owned)
            .ok_or_else(sign_in);
    }
    let refresh = text(&login, "refresh_token").ok_or_else(sign_in)?;
    let client = text(&login, "client_id")
        .filter(|id| *id != "dynamic_agent_client")
        .ok_or_else(sign_in)?;
    let updated = token_request(
        endpoints,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client),
            ("refresh_token", refresh),
            ("resource", "https://api.openai.com/v1"),
        ],
    )
    .map_err(|_| sign_in())?;
    let access = text(&updated, "access_token")
        .ok_or_else(sign_in)?
        .to_owned();
    text(&updated, "refresh_token").ok_or_else(sign_in)?;
    if text(&updated, "token_type").is_none_or(|kind| !kind.eq_ignore_ascii_case("Bearer")) {
        return Err(sign_in());
    }
    let lifetime = updated["expires_in"]
        .as_i64()
        .filter(|n| *n > 0 && *n <= 86400)
        .ok_or_else(sign_in)?;
    if updated.get("scope").is_some_and(|scope| {
        scope.as_str().is_none_or(|s| {
            !s.split_whitespace()
                .any(|s| s == "chatgpt.tokens.use.direct")
        })
    }) {
        return Err(sign_in());
    }
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "token_type",
        "scope",
        "expires_in",
        "earliest_refresh_at",
    ] {
        if let Some(value) = updated.get(key) {
            login[key] = value.clone();
        }
    }
    login["expires_at"] = (chrono::Utc::now().timestamp() + lifetime).into();
    logins
        .write(&account.account_id, &login)
        .map_err(|_| Failure::upstream("Could not save the refreshed inference login"))?;
    Ok(access)
}

fn text<'a>(login: &'a Value, key: &str) -> Option<&'a str> {
    login
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}
