use serde_json::Value;

use crate::anthropic_client::AnthropicClient;
use crate::env::Environment;
use crate::error::Result;
use crate::paths::Paths;
use crate::secret_store::SecretStore;
use crate::store::{self, find_account_slot};
use crate::usage_collector::{AccountInfo, UsageCollector};
use crate::usage_store::UsageEntry;

#[derive(Debug, Clone, PartialEq)]
pub enum StatusReport {
    NoActiveAccount,
    Unmanaged {
        email: String,
    },
    Managed {
        number: i64,
        email: String,
        organization_name: String,
        organization_uuid: String,
        alias: String,
        total_managed: usize,
        usage: Box<UsageEntry>,
    },
}

/// Which managed account, if any, the live login is, with its usage.
pub fn status(
    paths: &Paths,
    env: &Environment,
    secrets: &SecretStore,
    client: &AnthropicClient,
) -> Result<StatusReport> {
    let Some(identity) = store::current_identity(paths) else {
        return Ok(StatusReport::NoActiveAccount);
    };
    let unmanaged = || StatusReport::Unmanaged {
        email: identity.email.clone(),
    };
    let Some(data) = store::read_sequence_migrated(paths)?.filter(|d| !d.is_empty()) else {
        return Ok(unmanaged());
    };
    let Some(num) = find_account_slot(&data, &identity.email, &identity.organization_uuid)
        .filter(|n| !n.is_empty())
    else {
        return Ok(unmanaged());
    };
    let accounts = store::accounts(&data).expect("a slot was found in accounts");
    let account = &accounts[&num];
    let text = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let active = secrets.read_active();
    let info = AccountInfo {
        number: Value::from(num.parse::<i64>().unwrap_or_default()),
        num: num.clone(),
        email: identity.email.clone(),
        org_name: String::new(),
        org_uuid: identity.organization_uuid.clone(),
        alias: String::new(),
        is_active: true,
        creds: active.value.clone().unwrap_or_default(),
        unreadable: active.keychain_unavailable || active.value.is_none(),
    };
    let mut entries =
        UsageCollector::new(paths, env, secrets, client).collect(&[info], Some(&active));
    Ok(StatusReport::Managed {
        number: num.parse().unwrap_or_default(),
        email: identity.email.clone(),
        organization_name: text("organizationName"),
        organization_uuid: text("organizationUuid"),
        alias: text("alias"),
        total_managed: accounts.len(),
        usage: Box::new(entries.remove(&num).unwrap_or_default()),
    })
}
