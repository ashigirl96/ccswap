//! One store-backed usage pass over a set of accounts (upstream
//! `_collect_usage_entries`); fetches run outside every lock.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use ccswap_core::env::Environment;
use ccswap_core::paths::Paths;
use ccswap_core::poll_policy::{PlanInputs, plan_after_fetch};
use ccswap_core::settings::{SettingValue, Settings};
use ccswap_core::usage::{Sentinel, truthy};
use ccswap_core::usage_store::{FetchRecord, Identity, UsageEntry, UsageStore};
use serde_json::Value;

use crate::anthropic_client::{
    AnthropicClient, UsageOutcome, credential_fingerprint, extract_oauth_data,
    is_oauth_token_expired,
};
use crate::consume_gate::ConsumeGate;
use crate::secret_store::{
    ActiveCredentials, SecretStore, extract_access_token, looks_like_api_key,
};
use crate::session_profile;
use crate::store::{self, JsonObject};

mod active;

/// Delay between request starts so N accounts never hit the endpoint at once.
const FETCH_STAGGER: Duration = Duration::from_millis(250);

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

/// Every account in `sequence` order, with the live login resolved to its slot.
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
        self.collect_with(infos, active, None, false)
    }

    /// `fetch = None` makes every account a candidate under the persisted
    /// poll plans; a set restricts the candidates and lets them beat the
    /// plans (an empty set reads the store only). `scheduled` keeps valid
    /// future plans while still letting due ones fetch.
    pub fn collect_with(
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
        let fetcher = Fetcher {
            paths: &self.paths,
            secrets: self.secrets,
            client: self.client,
            gate: ConsumeGate {
                paths: &self.paths,
                env: self.env,
                secrets: self.secrets,
                client: self.client,
            },
            degraded: active.is_some_and(|a| a.degraded),
            unmirrored: self
                .env
                .non_empty("CLAUDE_SECURESTORAGE_CONFIG_DIR")
                .is_some(),
        };
        let fetcher = &fetcher;
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
                        (num.clone(), fetcher.fetch_account(info))
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

struct Fetcher<'f> {
    paths: &'f Paths,
    secrets: &'f SecretStore,
    client: &'f AnthropicClient,
    gate: ConsumeGate<'f>,
    /// The active read fell back past an unreadable Keychain.
    degraded: bool,
    unmirrored: bool,
}

impl Fetcher<'_> {
    /// Never fails: problems become records.
    fn fetch_account(&self, info: &AccountInfo) -> FetchRecord {
        if info.is_active {
            return self.fetch_active(info);
        }
        let client = self.client;
        let dir = session_profile::session_dir_for(self.paths, &info.num, &info.email);
        let mut live =
            !session_profile::live_session_pids(self.paths, &info.num, &info.email).is_empty();
        // Once a session has run, the profile holds the account's newest token
        // family; it is read strictly read-only so the next `run` stays logged in.
        let mut session_creds = session_profile::read_credentials(self.secrets.keychain(), &dir)
            .filter(|c| !c.is_empty());
        if session_creds.is_some()
            && session_profile::identity_drifted(&dir, &info.email, &info.org_uuid)
        {
            ccswap_core::log::debug(format!(
                "Session profile for account {} is logged in as a different account; fetching usage from the backup credential",
                info.num
            ));
            session_creds = None;
            live = false;
        }
        if let Some(session_creds) = session_creds
            && let Some(oauth) = extract_oauth_data(&session_creds)
                .filter(|o| o.get("accessToken").is_some_and(truthy))
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
        let gate = |num: &str, email: &str, snapshot: &str| {
            self.gate.consume_backup_grant(num, email, snapshot)
        };
        record_of(client.try_fetch_usage_for_account(
            &info.num,
            &info.email,
            &info.creds,
            live,
            if live { None } else { Some(&gate) },
        ))
    }
}
/// `autoswitch.threshold` and the `autoswitch.model` names, as upstream's
/// `_poll_policy_inputs` reads them from settings.json.
pub fn poll_policy_inputs(paths: &Paths) -> (f64, Vec<String>) {
    let settings = Settings::new(paths, crate::SETTINGS);
    let threshold = match settings.get("autoswitch.threshold").map(|s| s.value) {
        Ok(SettingValue::Float(f)) => f,
        Ok(SettingValue::Int(i)) => i as f64,
        _ => 90.0,
    };
    (threshold, crate::autoswitch::autoswitch_models(&settings))
}

/// A `[0, 1)` source for poll-plan jitter.
fn jitter_source() -> impl FnMut() -> f64 {
    let mut state =
        ccswap_core::usage::now_secs().to_bits() ^ (u64::from(std::process::id()) << 32);
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn static_active_sentinel(active: &ActiveCredentials) -> Option<Sentinel> {
    let creds = active.value.as_deref().unwrap_or_default();
    static_sentinel(creds, || {
        active.keychain_unavailable || active.value.is_none()
    })
}

/// The network-free verdict for a slot's credential; `unreadable` says
/// whether its store could not be read.
pub fn static_sentinel(creds: &str, unreadable: impl FnOnce() -> bool) -> Option<Sentinel> {
    if looks_like_api_key(creds) {
        return Some(Sentinel::ApiKey);
    }
    if creds.is_empty() || extract_access_token(creds).is_none_or(|t| t.is_empty()) {
        if unreadable() {
            return Some(Sentinel::KeychainUnavailable);
        }
        return Some(Sentinel::NoCredentials);
    }
    None
}
