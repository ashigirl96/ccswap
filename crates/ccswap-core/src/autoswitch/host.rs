//! What the engine needs from the store and the switch: the port every
//! frontend hands it ([`SwitcherHost`] in production, an in-memory fake in
//! the engine's own tests).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use crate::accounts::{Accounts, Console};
use crate::anthropic_client::{AnthropicClient, RefreshOutcome};
use crate::env::Environment;
use crate::error::Result;
use crate::paths::Paths;
use crate::presentation::Style;
use crate::secret_store::SecretStore;
use crate::snapshot::AccountKind;
use crate::store::{self, JsonObject};
use crate::switcher::Switcher;
use crate::usage_collector::{UsageCollector, build_accounts_info};
use crate::usage_store::UsageEntry;

/// The result of an engine-driven switch (`switch_to(.., json=True)`).
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchResult {
    pub switched: bool,
    pub reason: String,
    /// `{"number", "email"}` refs as `switch --json` reports them.
    pub from: Option<Value>,
    pub to: Option<Value>,
    pub warnings: Vec<String>,
}

pub trait EngineHost {
    /// Where `autoswitch_state.json` lives.
    fn backup_root(&self) -> PathBuf;
    /// Slot of the live login; `None` when there is none or it is unmanaged.
    fn current_account_number(&self) -> Option<String>;
    fn has_live_login(&self) -> bool;
    /// Stored email of a slot; empty when unknown.
    fn account_email(&self, num: &str) -> String;
    fn account_kind(&self, num: &str) -> AccountKind;
    /// Rotation order, without disabled slots or slots lacking backups.
    fn switchable_account_numbers(&self) -> Vec<String>;
    /// One store-backed entry per managed slot, in sequence order; only
    /// slots in `fetch` may be fetched.
    fn usage_entries(&self, fetch: &HashSet<String>, scheduled: bool) -> Vec<(String, UsageEntry)>;
    /// The slot's backup credential; empty when missing.
    fn read_account_credentials(&self, num: &str, email: &str) -> String;
    /// PIDs of live `run` sessions on this slot.
    fn live_session_pids_for(&self, num: &str, email: &str) -> Vec<i32>;
    /// Refreshes a slot's backup credential under the consume gate.
    fn consume_backup_grant(&self, num: &str, email: &str, snapshot: &str) -> RefreshOutcome;
    fn switch_to(&self, num: &str) -> Result<SwitchResult>;
    /// Pins the threshold and models that poll plans are computed with.
    fn set_poll_policy_inputs(&self, threshold: f64, models: &[String]);
}

struct SilentConsole;

impl Console for SilentConsole {
    fn line(&mut self, _text: &str) {}
    fn prompt(&mut self, _question: &str) -> Option<String> {
        None
    }
}

/// The production host over the store, the usage collector and the switch.
pub struct SwitcherHost<'a> {
    pub paths: &'a Paths,
    pub env: &'a Environment,
    pub secrets: &'a SecretStore,
    pub client: &'a AnthropicClient,
    pub style: &'a Style,
    poll_inputs: Mutex<Option<(f64, Vec<String>)>>,
}

impl<'a> SwitcherHost<'a> {
    pub fn new(
        paths: &'a Paths,
        env: &'a Environment,
        secrets: &'a SecretStore,
        client: &'a AnthropicClient,
        style: &'a Style,
    ) -> Self {
        SwitcherHost {
            paths,
            env,
            secrets,
            client,
            style,
            poll_inputs: Mutex::new(None),
        }
    }

    fn data(&self) -> JsonObject {
        store::read_sequence(self.paths)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn record_text(&self, num: &str, key: &str) -> Option<String> {
        let data = self.data();
        store::accounts(&data)?
            .get(num)?
            .get(key)?
            .as_str()
            .map(str::to_string)
    }
}

impl EngineHost for SwitcherHost<'_> {
    fn backup_root(&self) -> PathBuf {
        self.paths.backup_root()
    }

    fn current_account_number(&self) -> Option<String> {
        let identity = store::current_identity(self.paths)?;
        store::find_account_slot(&self.data(), &identity.email, &identity.organization_uuid)
    }

    fn has_live_login(&self) -> bool {
        store::current_identity(self.paths).is_some()
    }

    fn account_email(&self, num: &str) -> String {
        self.record_text(num, "email").unwrap_or_default()
    }

    fn account_kind(&self, num: &str) -> AccountKind {
        if self.record_text(num, "kind").as_deref() == Some("api_key") {
            AccountKind::ApiKey
        } else {
            AccountKind::OAuth
        }
    }

    fn switchable_account_numbers(&self) -> Vec<String> {
        Accounts {
            paths: self.paths,
            env: self.env,
            secrets: self.secrets,
            client: self.client,
            style: self.style,
        }
        .switchable_account_numbers()
        .unwrap_or_default()
    }

    fn usage_entries(&self, fetch: &HashSet<String>, scheduled: bool) -> Vec<(String, UsageEntry)> {
        let data = self.data();
        let (infos, active) = build_accounts_info(self.paths, self.secrets, &data);
        let mut collector = UsageCollector::new(self.paths, self.env, self.secrets, self.client);
        if let Some((threshold, models)) = self
            .poll_inputs
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
        {
            collector.threshold = threshold;
            collector.models = models;
        }
        let mut entries = collector.collect_with(&infos, active.as_ref(), Some(fetch), scheduled);
        infos
            .iter()
            .map(|i| (i.num.clone(), entries.remove(&i.num).unwrap_or_default()))
            .collect()
    }

    fn read_account_credentials(&self, num: &str, email: &str) -> String {
        self.secrets.read_backup(num, email).value
    }

    fn live_session_pids_for(&self, num: &str, email: &str) -> Vec<i32> {
        crate::session_profile::live_session_pids(self.paths, num, email)
    }

    /// The consume gate does not exist yet: every refresh is declined as
    /// busy, so nothing is consumed.
    fn consume_backup_grant(&self, _num: &str, _email: &str, _snapshot: &str) -> RefreshOutcome {
        RefreshOutcome {
            error: Some("consume-busy".into()),
            ..Default::default()
        }
    }

    fn switch_to(&self, num: &str) -> Result<SwitchResult> {
        let switcher = Switcher {
            paths: self.paths,
            env: self.env,
            secrets: self.secrets,
            client: self.client,
            style: self.style,
            listing: None,
        };
        let report = switcher.switch_to(num, true, false, &mut SilentConsole)?;
        Ok(match report {
            Some(report) => {
                let json = report.to_json();
                let reference = |key: &str| Some(json[key].clone()).filter(|v| !v.is_null());
                SwitchResult {
                    switched: report.switched,
                    reason: report.reason.to_string(),
                    from: reference("from"),
                    to: reference("to"),
                    warnings: report.warnings,
                }
            }
            None => SwitchResult {
                switched: false,
                reason: String::new(),
                from: None,
                to: None,
                warnings: Vec::new(),
            },
        })
    }

    fn set_poll_policy_inputs(&self, threshold: f64, models: &[String]) {
        if let Ok(mut g) = self.poll_inputs.lock() {
            *g = Some((threshold, models.to_vec()));
        }
    }
}
