//! One store-backed usage pass over a set of accounts (upstream
//! `_collect_usage_entries`): network-free verdicts first, then a reserve
//! of the slots that are due, parallel fetches outside every lock, and a
//! claim-fenced record. Everything else is served from `cache/usage.json`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::anthropic_client::{
    AnthropicClient, RefreshOutcome, UsageOutcome, credential_fingerprint, extract_oauth_data,
    is_oauth_token_expired,
};
use crate::env::Environment;
use crate::paths::Paths;
use crate::poll_policy::{PlanInputs, plan_after_fetch};
use crate::secret_store::{ActiveCredentials, Keychain, SecretStore};
use crate::session_profile;
use crate::settings::{SettingValue, Settings};
use crate::store::{self, JsonObject};
use crate::usage::{Sentinel, static_sentinel, truthy};
use crate::usage_store::{FetchRecord, Identity, UsageEntry, UsageStore};

/// Delay between request starts so N accounts never hit the endpoint at once.
const FETCH_STAGGER: Duration = Duration::from_millis(250);

/// A managed account as a usage pass sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountInfo {
    /// The slot as `sequence` stores it (normally an integer).
    pub number: Value,
    pub num: String,
    pub email: String,
    pub org_name: String,
    pub org_uuid: String,
    pub alias: String,
    pub is_active: bool,
    /// The live credential for the active slot, the backup otherwise.
    pub creds: String,
    /// The slot's store could not be read (not merely empty).
    pub unreadable: bool,
}

impl AccountInfo {
    pub fn identity(&self) -> Identity {
        (self.email.clone(), self.org_uuid.clone())
    }
}

/// Every account in `sequence` order, with the live login resolved to its
/// slot; `active` is the live-credential read when a slot is active.
pub fn build_accounts_info(
    paths: &Paths,
    secrets: &SecretStore,
    data: &JsonObject,
) -> (Vec<AccountInfo>, Option<ActiveCredentials>) {
    let active_num = store::current_identity(paths)
        .and_then(|id| store::find_account_slot(data, &id.email, &id.organization_uuid));
    let accounts = store::accounts(data);
    let mut active_read = None;
    let infos = data
        .get("sequence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|number| {
            let num = match number {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let account = accounts
                .and_then(|a| a.get(&num))
                .and_then(Value::as_object);
            let text = |key: &str, default: &str| {
                account
                    .and_then(|a| a.get(key))
                    .and_then(Value::as_str)
                    .unwrap_or(default)
                    .to_string()
            };
            let email = account
                .and_then(|a| a.get("email"))
                .map_or("unknown".to_string(), |v| {
                    v.as_str().map_or_else(|| v.to_string(), str::to_string)
                });
            let is_active = active_num.as_deref() == Some(num.as_str());
            let (creds, unreadable) = if is_active {
                let active = secrets.read_active();
                let creds = active.value.clone().unwrap_or_default();
                let unreadable = active.keychain_unavailable || active.value.is_none();
                active_read = Some(active);
                (creds, unreadable)
            } else {
                let backup = secrets.read_backup(&num, &email);
                (backup.value, backup.unreadable)
            };
            AccountInfo {
                number: number.clone(),
                num,
                org_name: text("organizationName", ""),
                org_uuid: text("organizationUuid", ""),
                alias: text("alias", ""),
                email,
                is_active,
                creds,
                unreadable,
            }
        })
        .collect();
    (infos, active_read)
}

/// The refresh gate inactive slots go through. Until the consume gate
/// exists, every refresh is declined as `consume-busy`: the expired token
/// is not sent and nothing is consumed.
fn decline_refresh(_num: &str, _email: &str, _snapshot: &str) -> RefreshOutcome {
    RefreshOutcome {
        error: Some("consume-busy".into()),
        ..Default::default()
    }
}

pub struct UsageCollector<'a> {
    pub paths: Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
    pub store: UsageStore,
    pub threshold: f64,
    pub models: Vec<String>,
}

impl<'a> UsageCollector<'a> {
    pub fn new(
        paths: &Paths,
        env: &'a Environment,
        secrets: &'a SecretStore,
        client: &'a AnthropicClient,
    ) -> Self {
        let (threshold, models) = poll_policy_inputs(paths);
        UsageCollector {
            paths: paths.clone(),
            env,
            secrets,
            client,
            store: UsageStore::new(&paths.usage_cache_dir()),
            threshold,
            models,
        }
    }

    /// One entry per account; fetches only what the store says is due.
    pub fn collect(
        &self,
        infos: &[AccountInfo],
        active: Option<&ActiveCredentials>,
    ) -> HashMap<String, UsageEntry> {
        self.collect_restricted(infos, active, None, false)
    }

    /// Like [`collect`](Self::collect), but only slots in `fetch` may be
    /// fetched, and they may beat the serve TTL (the auto engine's
    /// scheduler). `scheduled` keeps valid future plans while still letting
    /// due ones fetch.
    pub fn collect_restricted(
        &self,
        infos: &[AccountInfo],
        active: Option<&ActiveCredentials>,
        fetch: Option<&HashSet<String>>,
        scheduled: bool,
    ) -> HashMap<String, UsageEntry> {
        let identities: Vec<(String, Identity)> = infos
            .iter()
            .map(|i| (i.num.clone(), i.identity()))
            .collect();
        let mut sentinels: HashMap<String, Sentinel> = infos
            .iter()
            .filter_map(|i| Some((i.num.clone(), static_sentinel(&i.creds, || i.unreadable)?)))
            .collect();

        let mut entries = self.store.entries(&identities, &self.models);
        for info in infos {
            if sentinels.contains_key(&info.num) {
                continue;
            }
            let entry = &entries[&info.num];
            if self.entry_token_dead(entry, info) {
                sentinels.insert(info.num.clone(), Sentinel::ReloginRequired);
            } else if entry.auth_dead_strikes > 0 && entry.token_dead(None) {
                let _ = self
                    .store
                    .clear_dead_token(&[(info.num.clone(), info.identity())]);
                entries = self.store.entries(&identities, &self.models);
            }
        }

        let requested: Vec<String> = infos
            .iter()
            .filter(|i| !sentinels.contains_key(&i.num))
            .filter(|i| fetch.is_none_or(|f| f.contains(&i.num)))
            .map(|i| i.num.clone())
            .collect();
        let (respect_plans, repair_overslept) = match fetch {
            None => (true, true),
            Some(_) => (false, scheduled),
        };
        let claims = self
            .store
            .reserve(&requested, &identities, respect_plans, repair_overslept)
            .unwrap_or_default();

        for info in infos.iter().filter(|i| i.is_active) {
            if sentinels.contains_key(&info.num) || claims.iter().any(|(n, _)| *n == info.num) {
                continue;
            }
            if extract_oauth_data(&info.creds)
                .is_some_and(|o| is_oauth_token_expired(o.get("expiresAt")))
            {
                sentinels.insert(info.num.clone(), Sentinel::TokenExpired);
            }
        }

        if !claims.is_empty() {
            let pre = entries.clone();
            let records = self.run_fetches(infos, &claims, active);
            let plans = self.plans_after_fetch(&records, &pre, infos);
            let claim_map: HashMap<String, String> = claims.iter().cloned().collect();
            let accepted = self
                .store
                .record(&records, &identities, Some(&claim_map), &plans)
                .unwrap_or_default();
            for (num, record) in &records {
                if let Some(s) = record.sentinel.filter(|_| accepted.contains(num)) {
                    sentinels.insert(num.clone(), s);
                }
            }
            entries = self.store.entries(&identities, &self.models);
            for info in infos.iter().filter(|i| accepted.contains(&i.num)) {
                if self.entry_token_dead(&entries[&info.num], info) {
                    sentinels.insert(info.num.clone(), Sentinel::ReloginRequired);
                }
            }
        }

        infos
            .iter()
            .map(|info| {
                let mut entry = entries.remove(&info.num).unwrap_or_default();
                if let Some(s) = sentinels.get(&info.num) {
                    entry.sentinel = Some(*s);
                }
                (info.num.clone(), entry)
            })
            .collect()
    }

    /// The strike holds while any stored source still carries the struck
    /// generation; the active slot's backup is a second source.
    fn entry_token_dead(&self, entry: &UsageEntry, info: &AccountInfo) -> bool {
        if entry.token_dead(credential_fingerprint(&info.creds).as_deref()) {
            return true;
        }
        if !info.is_active {
            return false;
        }
        let backup = self.secrets.read_backup(&info.num, &info.email);
        if backup.unreadable {
            return entry.token_dead(None);
        }
        !backup.value.is_empty()
            && entry.token_dead(credential_fingerprint(&backup.value).as_deref())
    }

    fn run_fetches(
        &self,
        infos: &[AccountInfo],
        claims: &[(String, String)],
        active: Option<&ActiveCredentials>,
    ) -> Vec<(String, FetchRecord)> {
        let degraded = active.is_some_and(|a| a.degraded);
        let unmirrored = self
            .env
            .non_empty("CLAUDE_SECURESTORAGE_CONFIG_DIR")
            .is_some();
        let client = self.client;
        let sessions = SessionSource {
            paths: &self.paths,
            keychain: self.secrets.keychain(),
        };
        let sessions = &sessions;
        std::thread::scope(|scope| {
            let handles: Vec<_> = claims
                .iter()
                .enumerate()
                .map(|(idx, (num, _))| {
                    let info = infos.iter().find(|i| i.num == *num).expect("claimed slot");
                    scope.spawn(move || {
                        if idx > 0 {
                            std::thread::sleep(FETCH_STAGGER * idx as u32);
                        }
                        (
                            num.clone(),
                            fetch_account(client, info, degraded, unmirrored, sessions),
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("usage fetch thread"))
                .collect()
        })
    }

    fn plans_after_fetch(
        &self,
        records: &[(String, FetchRecord)],
        pre: &HashMap<String, UsageEntry>,
        infos: &[AccountInfo],
    ) -> HashMap<String, (f64, f64)> {
        let now = self.store.now();
        let mut rng = jitter_source();
        records
            .iter()
            .filter(|(_, r)| r.sentinel.is_none() && r.error.is_none())
            .map(|(num, record)| {
                let before = pre.get(num);
                let is_active = infos.iter().any(|i| i.num == *num && i.is_active);
                let plan = plan_after_fetch(
                    &PlanInputs {
                        prev_interval_s: before.and_then(|b| b.poll_interval_s),
                        prev_usage: before.and_then(|b| b.last_good.as_ref()),
                        new_usage: record.usage.as_ref(),
                        is_active,
                        threshold: self.threshold,
                        models: &self.models,
                        recent_429: before.is_some_and(|b| b.recent_429(now)),
                        now,
                    },
                    &mut rng,
                );
                (num.clone(), plan)
            })
            .collect()
    }
}

fn record_of(outcome: UsageOutcome) -> FetchRecord {
    FetchRecord {
        usage: outcome.usage,
        error: outcome.error,
        retry_after_s: outcome.retry_after_s,
        sentinel: None,
        struck_fp: outcome.struck_fp,
    }
}

/// Where an inactive slot's session profile lives.
struct SessionSource<'s> {
    paths: &'s Paths,
    keychain: &'s Keychain,
}

/// One network fetch for one slot. Never fails: problems become records.
fn fetch_account(
    client: &AnthropicClient,
    info: &AccountInfo,
    degraded: bool,
    unmirrored: bool,
    sessions: &SessionSource<'_>,
) -> FetchRecord {
    if info.is_active {
        return fetch_active(client, info, degraded, unmirrored);
    }
    let dir = session_profile::session_dir_for(sessions.paths, &info.num, &info.email);
    let mut live =
        !session_profile::live_session_pids(sessions.paths, &info.num, &info.email).is_empty();
    // Once a session has run, the profile holds the account's newest token
    // family; it is read strictly read-only so the next `run` stays logged in.
    let mut session_creds =
        session_profile::read_credentials(sessions.keychain, &dir).filter(|c| !c.is_empty());
    if session_creds.is_some()
        && session_profile::identity_drifted(&dir, &info.email, &info.org_uuid)
    {
        session_creds = None;
        live = false;
    }
    if let Some(session_creds) = session_creds
        && let Some(oauth) =
            extract_oauth_data(&session_creds).filter(|o| o.get("accessToken").is_some_and(truthy))
    {
        if !is_oauth_token_expired(oauth.get("expiresAt")) {
            return record_of(client.try_fetch_usage_for_account(
                &info.num,
                &info.email,
                &session_creds,
                true,
                None,
            ));
        }
        if live {
            return FetchRecord::sentinel(Sentinel::TokenExpired);
        }
    }
    record_of(client.try_fetch_usage_for_account(
        &info.num,
        &info.email,
        &info.creds,
        live,
        if live { None } else { Some(&decline_refresh) },
    ))
}

/// The active slot is fetched as-is. Recovering an expired or
/// server-rejected token under Claude Code's lock protocol is not done
/// here; such a pass defers with the 401 or "token expired".
fn fetch_active(
    client: &AnthropicClient,
    info: &AccountInfo,
    degraded: bool,
    unmirrored: bool,
) -> FetchRecord {
    let Some(oauth) =
        extract_oauth_data(&info.creds).filter(|o| o.get("accessToken").is_some_and(truthy))
    else {
        return FetchRecord::sentinel(Sentinel::NoCredentials);
    };
    let mut rejected = None;
    if !is_oauth_token_expired(oauth.get("expiresAt")) {
        let outcome =
            client.try_fetch_usage_for_account(&info.num, &info.email, &info.creds, true, None);
        if outcome.error.as_deref() != Some("http-401") {
            return record_of(outcome);
        }
        rejected = Some(FetchRecord::error("http-401", outcome.retry_after_s));
    }
    if degraded {
        return rejected.unwrap_or(FetchRecord::sentinel(Sentinel::KeychainUnavailable));
    }
    if unmirrored {
        return FetchRecord::error("store-unmirrored", None);
    }
    rejected.unwrap_or(FetchRecord::sentinel(Sentinel::TokenExpired))
}

/// `autoswitch.threshold` and the `autoswitch.model` names, as upstream's
/// `_poll_policy_inputs` reads them from settings.json.
pub fn poll_policy_inputs(paths: &Paths) -> (f64, Vec<String>) {
    let settings = Settings::new(paths);
    let threshold = match settings.get("autoswitch.threshold").map(|s| s.value) {
        Ok(SettingValue::Float(f)) => f,
        Ok(SettingValue::Int(i)) => i as f64,
        _ => 90.0,
    };
    (threshold, settings.autoswitch_models())
}

/// A `[0, 1)` source for poll-plan jitter.
fn jitter_source() -> impl FnMut() -> f64 {
    let mut state = crate::usage::now_secs().to_bits() ^ (u64::from(std::process::id()) << 32);
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    }
}
