use ccswap_core::env::Environment;
use ccswap_core::error::Result;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::{SCHEMA_VERSION, Style};
use ccswap_core::usage_store::UsageEntry;
use ccswap_core::usage_view::insert_usage_fields;
use serde_json::{Map, Value, json};

use crate::anthropic_client::AnthropicClient;
use crate::secret_store::SecretStore;
use crate::store::{self, find_account_slot};
use crate::usage_collector::{AccountInfo, UsageCollector};
use crate::usage_view::usage_entry_lines;

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

pub fn status_lines(report: &StatusReport, style: &Style) -> Vec<String> {
    let label = style.bolded("Status:");
    match report {
        StatusReport::NoActiveAccount => {
            vec![format!(
                "{label} {}",
                style.dimmed("No active Claude account")
            )]
        }
        StatusReport::Unmanaged { email } => {
            vec![format!("{label} {email} {}", style.dimmed("(not managed)"))]
        }
        StatusReport::Managed {
            number,
            email,
            organization_name,
            total_managed,
            usage,
            ..
        } => {
            let tag = if organization_name.is_empty() {
                "personal"
            } else {
                organization_name
            };
            let mut lines = vec![
                format!(
                    "{label} {} ({email} {})",
                    style.accent(&format!("Account-{number}")),
                    style.muted(&format!("[{tag}]"))
                ),
                format!(
                    "  {}",
                    style.dimmed(&format!("Total managed accounts: {total_managed}"))
                ),
            ];
            lines.extend(
                usage_entry_lines(usage, style)
                    .into_iter()
                    .map(|l| format!("  {l}")),
            );
            lines
        }
    }
}

pub fn status_json(report: &StatusReport) -> Value {
    let mut payload = Map::new();
    payload.insert("schemaVersion".into(), SCHEMA_VERSION.into());
    match report {
        StatusReport::NoActiveAccount => {
            payload.insert("active".into(), Value::Null);
        }
        StatusReport::Unmanaged { email } => {
            payload.insert("active".into(), json!({ "email": email, "managed": false }));
        }
        StatusReport::Managed {
            number,
            email,
            organization_name,
            organization_uuid,
            alias,
            total_managed,
            usage,
        } => {
            let mut active = Map::new();
            active.insert("number".into(), (*number).into());
            active.insert("email".into(), email.as_str().into());
            active.insert("organizationName".into(), organization_name.as_str().into());
            active.insert("organizationUuid".into(), organization_uuid.as_str().into());
            active.insert(
                "isOrganization".into(),
                (!organization_uuid.is_empty()).into(),
            );
            active.insert("managed".into(), true.into());
            insert_usage_fields(&mut active, usage, alias, false);
            payload.insert("active".into(), Value::Object(active));
            payload.insert("totalManagedAccounts".into(), (*total_managed).into());
        }
    }
    Value::Object(payload)
}
