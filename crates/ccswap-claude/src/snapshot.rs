//! One coherent account snapshot per pass, and the reconciliation that keeps
//! a late, older pass from regressing what is on screen (upstream
//! `snapshot_source.py` and `accounts_snapshot`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ccswap_core::env::Environment;
use ccswap_core::error::Result;
use ccswap_core::paths::Paths;
use ccswap_core::usage::{Sentinel, truthy};
use ccswap_core::usage_store::UsageEntry;
use serde_json::Value;

use crate::actions::Actions;
use crate::anthropic_client::AnthropicClient;
use crate::secret_store::SecretStore;
use crate::store::{self, JsonObject};
use crate::usage_collector::{UsageCollector, build_accounts_info};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountKind {
    OAuth,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountSnapshot {
    pub number: String,
    pub email: String,
    pub org_name: String,
    pub org_uuid: String,
    pub is_active: bool,
    pub kind: AccountKind,
    pub switchable: bool,
    pub usage: UsageEntry,
    pub alias: String,
    pub disabled: bool,
}

impl AccountSnapshot {
    pub fn display_tag(&self) -> &str {
        if self.org_name.is_empty() {
            "personal"
        } else {
            &self.org_name
        }
    }

    /// `alias (email)`, or the bare email.
    pub fn display_name(&self) -> String {
        if self.alias.is_empty() {
            self.email.clone()
        } else {
            format!("{} ({})", self.alias, self.email)
        }
    }
}

/// Every managed account from one collect pass, in `sequence` order.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountsSnapshot {
    pub active_number: Option<String>,
    pub accounts: Vec<AccountSnapshot>,
    pub taken_at: f64,
}

/// The single definition of "same account" across snapshots: the source's
/// reconciliation and a surface's merge must agree on it.
pub fn account_identity(acc: &AccountSnapshot) -> (&str, &str, AccountKind) {
    (&acc.email, &acc.org_uuid, acc.kind)
}

/// `fetch = None` lets every stale account be fetched; `Some(empty)` reads
/// the store only.
pub fn accounts_snapshot(
    paths: &Paths,
    env: &Environment,
    secrets: &SecretStore,
    client: &AnthropicClient,
    fetch: Option<&HashSet<String>>,
    poll_inputs: Option<&(f64, Vec<String>)>,
) -> Result<AccountsSnapshot> {
    let data = if paths.sequence_file().exists() {
        store::read_sequence_migrated(paths)?.unwrap_or_default()
    } else {
        JsonObject::new()
    };
    let (infos, active) = build_accounts_info(paths, secrets, &data);
    let mut collector = UsageCollector::new(paths, env, secrets, client);
    if let Some((threshold, models)) = poll_inputs {
        collector.threshold = *threshold;
        collector.models = models.clone();
    }
    let mut entries = collector.collect_with(&infos, active.as_ref(), fetch, false);
    let taken_at = collector.store.now();
    let records = store::accounts(&data);
    let record = |num: &str| records.and_then(|a| a.get(num)).and_then(Value::as_object);
    let accounts: Vec<AccountSnapshot> = infos
        .into_iter()
        .map(|info| {
            let rec = record(&info.num);
            AccountSnapshot {
                kind: match rec.and_then(|r| r.get("kind")).and_then(Value::as_str) {
                    Some("api_key") => AccountKind::ApiKey,
                    _ => AccountKind::OAuth,
                },
                switchable: is_switchable(paths, secrets, rec, &info.num),
                disabled: rec.and_then(|r| r.get("disabled")).is_some_and(truthy),
                usage: entries.remove(&info.num).unwrap_or_default(),
                number: info.num,
                email: info.email,
                org_name: info.org_name,
                org_uuid: info.org_uuid,
                is_active: info.is_active,
                alias: info.alias,
            }
        })
        .collect();
    Ok(AccountsSnapshot {
        active_number: accounts
            .iter()
            .find(|a| a.is_active)
            .map(|a| a.number.clone()),
        accounts,
        taken_at,
    })
}

/// Both a stored credential and a stored config backup.
fn is_switchable(
    paths: &Paths,
    secrets: &SecretStore,
    record: Option<&JsonObject>,
    num: &str,
) -> bool {
    let Some(record) = record.filter(|r| !r.is_empty()) else {
        return false;
    };
    let email = record
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    !secrets.read_backup(num, email).value.is_empty()
        && !store::read_account_config(paths, num, email).is_empty()
}

/// Takes snapshots and reconciles each with the previous one. The usage
/// store paces the network, so an explicit refresh is no faster than a
/// normal pass. Blocking: call it off the UI thread.
pub struct SnapshotSource {
    actions: Arc<dyn Actions>,
    last: Mutex<Option<AccountsSnapshot>>,
}

impl SnapshotSource {
    pub fn new(actions: Arc<dyn Actions>) -> Self {
        SnapshotSource {
            actions,
            last: Mutex::new(None),
        }
    }

    pub fn take(&self, store_only: bool) -> Result<AccountsSnapshot> {
        let store_set = HashSet::new();
        let fetch = store_only.then_some(&store_set);
        let snap = self.actions.accounts_snapshot(fetch)?;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let snap = reconcile(last.as_ref(), snap);
        *last = Some(snap.clone());
        Ok(snap)
    }
}

fn reconcile(previous: Option<&AccountsSnapshot>, mut snap: AccountsSnapshot) -> AccountsSnapshot {
    let Some(previous) = previous else {
        return snap;
    };
    let by_number: HashMap<&str, &AccountSnapshot> = previous
        .accounts
        .iter()
        .map(|a| (a.number.as_str(), a))
        .collect();
    let taken_at = snap.taken_at;
    for acc in &mut snap.accounts {
        if let Some(prev) = by_number.get(acc.number.as_str()) {
            reconcile_account(acc, prev, taken_at);
        }
    }
    snap
}

fn reconcile_account(acc: &mut AccountSnapshot, prev: &AccountSnapshot, taken_at: f64) {
    if account_identity(acc) != account_identity(prev) {
        return;
    }
    let fetched = acc.usage.fetched_at;
    if let Some(prev_fetched) = prev.usage.fetched_at
        && fetched.is_none_or(|f| f < prev_fetched)
    {
        acc.usage = prev.usage.clone();
        acc.usage.age_s = Some((taken_at - prev_fetched).max(0.0));
        return;
    }
    if acc.usage.sentinel.is_some() {
        return;
    }
    if prev.usage.sentinel == Some(Sentinel::TokenExpired) && fetched == prev.usage.fetched_at {
        acc.usage.sentinel = Some(Sentinel::TokenExpired);
    }
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::TestSnapshotSource.
    use ccswap_core::console::Console;
    use ccswap_core::usage::now_secs as now;

    use super::*;
    use crate::switcher::SwitchStrategy;
    use serde_json::json;

    struct Fake {
        accounts: Mutex<Vec<AccountSnapshot>>,
        fetch_sets: Mutex<Vec<Option<HashSet<String>>>>,
    }

    impl Actions for Fake {
        fn backup_dir(&self) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        fn accounts_snapshot(&self, fetch: Option<&HashSet<String>>) -> Result<AccountsSnapshot> {
            self.fetch_sets.lock().unwrap().push(fetch.cloned());
            Ok(AccountsSnapshot {
                active_number: None,
                accounts: self.accounts.lock().unwrap().clone(),
                taken_at: now(),
            })
        }
        fn current_account_number(&self) -> Option<String> {
            None
        }
        fn switch_to(&self, _: &str, _: bool, _: &mut dyn Console) -> Result<Option<Value>> {
            Ok(None)
        }
        fn switch(&self, _: Option<&SwitchStrategy>, _: &mut dyn Console) -> Result<Option<Value>> {
            Ok(None)
        }
        fn remove_account(&self, _: &str, _: bool, _: &mut dyn Console) -> Result<()> {
            Ok(())
        }
        fn set_account_disabled(&self, _: &str, _: bool, _: &mut dyn Console) -> Result<()> {
            Ok(())
        }
        fn add_account(&self, _: Option<i64>, _: bool, _: &mut dyn Console) -> Result<()> {
            Ok(())
        }
        fn add_account_from_token(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<i64>,
            _: bool,
            _: &mut dyn Console,
        ) -> Result<()> {
            Ok(())
        }
        fn set_poll_policy_inputs(&self, _: f64, _: Vec<String>) {}
        fn clear_poll_policy_inputs(&self) {}
    }

    fn account(email: &str, kind: AccountKind, usage: UsageEntry) -> AccountSnapshot {
        AccountSnapshot {
            number: "1".into(),
            email: email.into(),
            org_name: String::new(),
            org_uuid: String::new(),
            is_active: true,
            kind,
            switchable: true,
            usage,
            alias: String::new(),
            disabled: false,
        }
    }

    fn usage_at(fetched_at: Option<f64>, pct: f64, sentinel: Option<Sentinel>) -> UsageEntry {
        UsageEntry {
            sentinel,
            last_good: Some(json!({"five_hour": {"pct": pct}})),
            fetched_at,
            age_s: fetched_at.map(|f| now() - f),
            ..Default::default()
        }
    }

    fn oauth(usage: UsageEntry) -> AccountSnapshot {
        account("user1@example.com", AccountKind::OAuth, usage)
    }

    fn source(first: AccountSnapshot) -> (Arc<Fake>, SnapshotSource) {
        let fake = Arc::new(Fake {
            accounts: Mutex::new(vec![first]),
            fetch_sets: Mutex::new(Vec::new()),
        });
        let source = SnapshotSource::new(fake.clone());
        (fake, source)
    }

    fn set(fake: &Fake, acc: AccountSnapshot) {
        *fake.accounts.lock().unwrap() = vec![acc];
    }

    #[test]
    fn every_pass_is_store_governed_and_store_only_never_fetches() {
        let (fake, source) = source(oauth(usage_at(None, 1.0, None)));
        source.take(false).unwrap();
        source.take(false).unwrap();
        source.take(true).unwrap();
        assert_eq!(
            *fake.fetch_sets.lock().unwrap(),
            vec![None, None, Some(HashSet::new())]
        );
    }

    #[test]
    fn expired_sentinel_retained_until_fetched_at_advances() {
        let expired = Some(Sentinel::TokenExpired);
        let (fake, source) = source(oauth(usage_at(Some(100.0), 25.0, expired)));
        assert_eq!(
            source.take(false).unwrap().accounts[0].usage.sentinel,
            expired
        );
        set(&fake, oauth(usage_at(Some(100.0), 25.0, None)));
        assert_eq!(
            source.take(true).unwrap().accounts[0].usage.sentinel,
            expired
        );
        set(&fake, oauth(usage_at(Some(101.0), 25.0, None)));
        assert_eq!(source.take(true).unwrap().accounts[0].usage.sentinel, None);
    }

    #[test]
    fn expired_sentinel_clears_on_superseding_sentinel() {
        let (fake, source) = source(oauth(usage_at(
            Some(100.0),
            25.0,
            Some(Sentinel::TokenExpired),
        )));
        source.take(false).unwrap();
        set(
            &fake,
            account(
                "user1@example.com",
                AccountKind::ApiKey,
                usage_at(None, 25.0, Some(Sentinel::ApiKey)),
            ),
        );
        assert_eq!(
            source.take(true).unwrap().accounts[0].usage.sentinel,
            Some(Sentinel::ApiKey)
        );
    }

    #[test]
    fn expired_sentinel_clears_on_identity_replacement() {
        let (fake, source) = source(account(
            "old@example.com",
            AccountKind::OAuth,
            usage_at(Some(100.0), 25.0, Some(Sentinel::TokenExpired)),
        ));
        source.take(false).unwrap();
        set(
            &fake,
            account(
                "new@example.com",
                AccountKind::OAuth,
                usage_at(Some(100.0), 25.0, None),
            ),
        );
        assert_eq!(source.take(true).unwrap().accounts[0].usage.sentinel, None);
    }

    #[test]
    fn late_worker_fetched_at_regression_is_rejected() {
        let (fake, source) = source(oauth(usage_at(Some(200.0), 80.0, None)));
        source.take(false).unwrap();
        set(&fake, oauth(usage_at(Some(100.0), 10.0, None)));
        let usage = source.take(true).unwrap().accounts[0].usage.clone();
        assert_eq!(usage.fetched_at, Some(200.0));
        assert_eq!(usage.last_good.unwrap()["five_hour"]["pct"], 80.0);
    }

    #[test]
    fn late_expired_sentinel_cannot_replace_newer_usage() {
        let (fake, source) = source(oauth(usage_at(Some(200.0), 80.0, None)));
        source.take(false).unwrap();
        set(
            &fake,
            oauth(usage_at(Some(100.0), 10.0, Some(Sentinel::TokenExpired))),
        );
        let usage = source.take(true).unwrap().accounts[0].usage.clone();
        assert_eq!(usage.sentinel, None);
        assert_eq!(usage.fetched_at, Some(200.0));
        assert_eq!(usage.last_good.unwrap()["five_hour"]["pct"], 80.0);
    }
}
