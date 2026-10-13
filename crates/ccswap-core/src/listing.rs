//! `list`: every managed account with its usage.

use serde_json::Value;

use crate::anthropic_client::AnthropicClient;
use crate::env::Environment;
use crate::error::Result;
use crate::paths::Paths;
use crate::secret_store::SecretStore;
use crate::store;
use crate::usage::truthy;
use crate::usage_collector::{AccountInfo, UsageCollector, build_accounts_info};
use crate::usage_store::UsageEntry;

#[derive(Debug, Clone, PartialEq)]
pub struct ListedAccount {
    pub info: AccountInfo,
    pub usage: UsageEntry,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ListReport {
    /// `sequence.json` does not exist.
    NoStore,
    Accounts(Vec<ListedAccount>),
}

impl ListReport {
    /// The live login's slot as `sequence` stores it.
    pub fn active_number(&self) -> Option<&Value> {
        match self {
            ListReport::NoStore => None,
            ListReport::Accounts(rows) => rows
                .iter()
                .find(|r| r.info.is_active)
                .map(|r| &r.info.number),
        }
    }
}

pub fn list(
    paths: &Paths,
    env: &Environment,
    secrets: &SecretStore,
    client: &AnthropicClient,
) -> Result<ListReport> {
    if !paths.sequence_file().exists() {
        return Ok(ListReport::NoStore);
    }
    let data = store::read_sequence_migrated(paths)?.unwrap_or_default();
    let (infos, active) = build_accounts_info(paths, secrets, &data);
    let mut entries =
        UsageCollector::new(paths, env, secrets, client).collect(&infos, active.as_ref());
    let accounts = store::accounts(&data);
    Ok(ListReport::Accounts(
        infos
            .into_iter()
            .map(|info| ListedAccount {
                usage: entries.remove(&info.num).unwrap_or_default(),
                disabled: accounts
                    .and_then(|a| a.get(&info.num))
                    .and_then(|a| a.get("disabled"))
                    .is_some_and(truthy),
                info,
            })
            .collect(),
    ))
}
