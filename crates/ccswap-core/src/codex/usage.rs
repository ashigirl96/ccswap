//! Codex usage from `GET <base>/backend-api/wham/usage`, cached under `codex/cache/` by the
//! same [`UsageStore`] the Claude side uses (TTL, backoff, 429 handling).
//!
//! ccswap never refreshes a Codex token: the refresh token rotates, and a refresh by ccswap
//! would leave Codex's copy `refresh_token_reused`. An expired access token goes through
//! [`CodexUsage::fetch_expired`] instead.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::anthropic_client::{AnthropicClient, classify_transport, parse_retry_after};
use crate::endpoints::Endpoints;
use crate::usage::{Sentinel, now_secs};
use crate::usage_store::{FetchRecord, Identity, UsageEntry, UsageStore};

use super::identity::jwt_claims;
use super::paths::CodexPaths;
use super::store::{CodexListing, CodexStore};

const USAGE_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_STAGGER: Duration = Duration::from_millis(250);
const USER_AGENT: &str = "ccswap";
/// Codex itself treats a token this close to `exp` as expired.
const EXPIRY_BUFFER_S: f64 = 300.0;

/// `(label, key in the normalised usage, field of wham's rate_limit)`.
pub const WINDOWS: [(&str, &str, &str); 2] = [
    ("5h", "five_hour", "primary_window"),
    ("7d", "seven_day", "secondary_window"),
];

/// The credential a fetch uses: the live `auth.json` for the active account, its managed home otherwise.
#[derive(Debug, Clone)]
pub struct CodexCredential {
    pub number: u32,
    pub active: bool,
    pub auth: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
enum AccessToken {
    Missing,
    Expired,
    Valid {
        access_token: String,
        account_id: String,
    },
}

fn access_token(auth: &[u8], fallback_account_id: &str) -> AccessToken {
    let Ok(auth) = serde_json::from_slice::<Value>(auth) else {
        return AccessToken::Missing;
    };
    let tokens = auth.get("tokens");
    let Some(token) = tokens
        .and_then(|t| t.get("access_token"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return AccessToken::Missing;
    };
    let exp = jwt_claims(token).and_then(|c| c.get("exp").and_then(Value::as_f64));
    if exp.is_some_and(|exp| now_secs() + EXPIRY_BUFFER_S >= exp) {
        return AccessToken::Expired;
    }
    let account_id = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_account_id);
    AccessToken::Valid {
        access_token: token.to_string(),
        account_id: account_id.to_string(),
    }
}

pub struct CodexUsage {
    store: UsageStore,
    usage_url: String,
}

impl CodexUsage {
    pub fn new(paths: &CodexPaths, endpoints: &Endpoints) -> Self {
        CodexUsage {
            store: UsageStore::new(&paths.usage_cache_dir()),
            usage_url: endpoints.codex_usage_url(),
        }
    }

    /// One entry per listed account. Syncs the live `auth.json` into its owner's home first,
    /// then fetches only the accounts the cache says are due.
    pub fn collect(&self, codex: &CodexStore, listing: &CodexListing) -> HashMap<u32, UsageEntry> {
        if listing.accounts.is_empty() {
            return HashMap::new();
        }
        if let Err(e) = codex.sync_live_owner() {
            crate::log::debug(format!("Could not sync the live Codex auth.json: {e}"));
        }
        let live = codex.read_live().ok().flatten();
        let identities: Vec<(String, Identity)> = listing
            .accounts
            .iter()
            .map(|a| {
                (
                    a.number.to_string(),
                    (a.email.clone(), a.account_id.clone()),
                )
            })
            .collect();
        let mut sentinels: HashMap<String, Sentinel> = HashMap::new();
        let mut credentials = Vec::new();
        for account in &listing.accounts {
            let auth = if account.active {
                live.clone()
            } else {
                codex.read_managed(account.number).ok().flatten()
            };
            match auth.filter(|a| access_token(a, "") != AccessToken::Missing) {
                Some(auth) => credentials.push(CodexCredential {
                    number: account.number,
                    active: account.active,
                    auth,
                }),
                None => {
                    sentinels.insert(account.number.to_string(), Sentinel::NoCredentials);
                }
            }
        }

        let requested: Vec<String> = credentials.iter().map(|c| c.number.to_string()).collect();
        let claims = self
            .store
            .reserve(&requested, &identities, true, true)
            .unwrap_or_default();
        if !claims.is_empty() {
            let records = self.run_fetches(listing, &credentials, &claims);
            let claim_map: HashMap<String, String> = claims.into_iter().collect();
            let accepted = self
                .store
                .record(&records, &identities, Some(&claim_map), &HashMap::new())
                .unwrap_or_default();
            for (num, record) in &records {
                if let Some(s) = record.sentinel.filter(|_| accepted.contains(num)) {
                    sentinels.insert(num.clone(), s);
                }
            }
        }

        let mut entries = self.store.entries(&identities, &[]);
        listing
            .accounts
            .iter()
            .map(|a| {
                let num = a.number.to_string();
                let mut entry = entries.remove(&num).unwrap_or_default();
                if let Some(s) = sentinels.get(&num) {
                    entry.sentinel = Some(*s);
                }
                (a.number, entry)
            })
            .collect()
    }

    fn run_fetches(
        &self,
        listing: &CodexListing,
        credentials: &[CodexCredential],
        claims: &[(String, String)],
    ) -> Vec<(String, FetchRecord)> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = claims
                .iter()
                .enumerate()
                .map(|(idx, (num, _))| {
                    let credential = credentials
                        .iter()
                        .find(|c| c.number.to_string() == *num)
                        .expect("claimed account has a credential");
                    let account_id = listing
                        .accounts
                        .iter()
                        .find(|a| a.number == credential.number)
                        .map(|a| a.account_id.clone())
                        .unwrap_or_default();
                    scope.spawn(move || {
                        if idx > 0 {
                            std::thread::sleep(FETCH_STAGGER * idx as u32);
                        }
                        (num.clone(), self.fetch_account(credential, &account_id))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("codex usage fetch thread"))
                .collect()
        })
    }

    /// Never fails: problems become records.
    fn fetch_account(&self, credential: &CodexCredential, account_id: &str) -> FetchRecord {
        match access_token(&credential.auth, account_id) {
            AccessToken::Missing => FetchRecord::sentinel(Sentinel::NoCredentials),
            AccessToken::Expired => self.fetch_expired(credential),
            AccessToken::Valid {
                access_token,
                account_id,
            } => self.fetch_wham(&access_token, &account_id),
        }
    }

    /// Where an expired access token's usage comes from; for now it only says so.
    fn fetch_expired(&self, _credential: &CodexCredential) -> FetchRecord {
        FetchRecord::sentinel(Sentinel::TokenExpired)
    }

    fn fetch_wham(&self, access_token: &str, account_id: &str) -> FetchRecord {
        let response = AnthropicClient::agent(USAGE_TIMEOUT)
            .get(&self.usage_url)
            .header("Authorization", &format!("Bearer {access_token}"))
            .header("ChatGPT-Account-Id", account_id)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT)
            .call();
        let mut response = match response {
            Ok(r) => r,
            Err(e) => return FetchRecord::error(classify_transport(&e).kind, None),
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let retry_after_s = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after);
            crate::log::debug(format!("Codex usage request failed: HTTP {status}"));
            return FetchRecord::error(format!("http-{status}"), retry_after_s);
        }
        let body = match response.body_mut().read_to_vec() {
            Ok(b) => b,
            Err(e) => return FetchRecord::error(classify_transport(&e).kind, None),
        };
        match serde_json::from_slice::<Value>(&body) {
            Ok(data) => FetchRecord {
                usage: Some(normalize_usage(&data)),
                ..Default::default()
            },
            Err(_) => FetchRecord::error("bad-response", None),
        }
    }
}

/// wham's body in the Claude side's normalised shape (`five_hour` / `seven_day` with `pct`
/// and an ISO `resets_at`). A window that is missing or malformed is left out: it is unknown.
pub fn normalize_usage(data: &Value) -> Value {
    let rate_limit = data.get("rate_limit");
    let mut out = Map::new();
    for (_, key, field) in WINDOWS {
        if let Some(window) = rate_limit.and_then(|r| r.get(field)).and_then(window) {
            out.insert(key.into(), window);
        }
    }
    Value::Object(out)
}

fn window(raw: &Value) -> Option<Value> {
    let pct = number(raw.get("used_percent")?)?;
    let mut out = Map::new();
    out.insert("pct".into(), json!(pct));
    if let Some(reset) = raw.get("reset_at").and_then(number).and_then(iso_utc) {
        out.insert("resets_at".into(), reset.into());
    }
    if let Some(seconds) = raw.get("limit_window_seconds").and_then(number) {
        out.insert("window_s".into(), json!(seconds));
    }
    Some(Value::Object(out))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn iso_utc(epoch_s: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(epoch_s as i64, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S+00:00").to_string())
}
