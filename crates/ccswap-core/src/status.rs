use serde_json::Value;

use crate::error::Result;
use crate::paths::Paths;
use crate::secret_store::SecretStore;
use crate::store::{self, find_account_slot};
use crate::usage::{UsageEntry, static_active_sentinel};

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
        usage: UsageEntry,
    },
}

/// Which managed account, if any, the live login is.
pub fn status(paths: &Paths, secrets: &SecretStore) -> Result<StatusReport> {
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
    let usage = match static_active_sentinel(&active) {
        Some(sentinel) => UsageEntry::Sentinel(sentinel),
        None => UsageEntry::Unavailable,
    };
    Ok(StatusReport::Managed {
        number: num.parse().unwrap_or_default(),
        email: identity.email.clone(),
        organization_name: text("organizationName"),
        organization_uuid: text("organizationUuid"),
        alias: text("alias"),
        total_managed: accounts.len(),
        usage,
    })
}
