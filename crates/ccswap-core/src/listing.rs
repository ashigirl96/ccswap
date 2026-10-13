use serde_json::Value;

use crate::anthropic_client::{AnthropicClient, build_token_status, credential_fingerprint};
use crate::env::Environment;
use crate::error::Result;
use crate::paths::Paths;
use crate::process_detection::{InstanceGroup, get_running_instances, group_instances};
use crate::secret_store::{Keychain, SecretStore, looks_like_api_key};
use crate::session_profile;
use crate::store::{self, JsonObject};
use crate::unclaimed::UnclaimedStash;
use crate::usage::truthy;
use crate::usage_collector::{AccountInfo, UsageCollector, build_accounts_info};
use crate::usage_store::{DecisionValue, UsageEntry};

#[derive(Debug, Clone, PartialEq)]
pub struct ListedAccount {
    pub info: AccountInfo,
    pub usage: UsageEntry,
    pub disabled: bool,
    /// Source-labelled token diagnostics; empty unless requested.
    pub token_status: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListedAccounts {
    pub rows: Vec<ListedAccount>,
    pub duplicate_warnings: Vec<String>,
    pub lockstep_warnings: Vec<String>,
    /// Human view only.
    pub instances: Vec<InstanceGroup>,
    /// Sorted stash ids; JSON only, since users cannot act on them.
    pub unclaimed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ListReport {
    /// `sequence.json` does not exist.
    NoStore,
    Accounts(ListedAccounts),
}

impl ListReport {
    /// The live login's slot as `sequence` stores it.
    pub fn active_number(&self) -> Option<&Value> {
        match self {
            ListReport::NoStore => None,
            ListReport::Accounts(listed) => listed
                .rows
                .iter()
                .find(|r| r.info.is_active)
                .map(|r| &r.info.number),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ListOptions {
    pub token_status: bool,
    pub json: bool,
}

pub fn list(
    paths: &Paths,
    env: &Environment,
    secrets: &SecretStore,
    client: &AnthropicClient,
    options: ListOptions,
) -> Result<ListReport> {
    if !paths.sequence_file().exists() {
        return Ok(ListReport::NoStore);
    }
    let data = store::read_sequence_migrated(paths)?.unwrap_or_default();
    let (infos, active) = build_accounts_info(paths, secrets, &data);
    let mut entries =
        UsageCollector::new(paths, env, secrets, client).collect(&infos, active.as_ref());
    let duplicate_warnings = duplicate_account_warnings(&infos, &data);
    let lockstep_warnings = lockstep_usage_warnings(&infos, &entries);
    let accounts = store::accounts(&data);
    let rows = infos
        .into_iter()
        .map(|info| ListedAccount {
            usage: entries.remove(&info.num).unwrap_or_default(),
            disabled: accounts
                .and_then(|a| a.get(&info.num))
                .and_then(|a| a.get("disabled"))
                .is_some_and(truthy),
            token_status: if options.token_status {
                token_status_lines(paths, secrets.keychain(), &info)
            } else {
                Vec::new()
            },
            info,
        })
        .collect();
    let (instances, unclaimed) = if options.json {
        let ids = UnclaimedStash::new(paths).list().into_keys().collect();
        (Vec::new(), ids)
    } else {
        let (sessions, ides) = get_running_instances(&paths.claude_config_home());
        let home = paths.home().display().to_string();
        (group_instances(&sessions, &ides, &home), Vec::new())
    };
    Ok(ListReport::Accounts(ListedAccounts {
        rows,
        duplicate_warnings,
        lockstep_warnings,
        instances,
        unclaimed,
    }))
}

fn label_token_status(source: &str, credentials: &str) -> Option<String> {
    let status = build_token_status(credentials)?;
    Some(format!(
        "{source}: {}",
        status.strip_prefix("oauth: ").unwrap_or(&status)
    ))
}

/// Read-only: never refreshes or writes any credential.
fn token_status_lines(paths: &Paths, keychain: &Keychain, info: &AccountInfo) -> Vec<String> {
    if looks_like_api_key(&info.creds) {
        return Vec::new();
    }
    if info.is_active {
        return label_token_status("active profile", &info.creds)
            .into_iter()
            .collect();
    }
    let mut lines = Vec::new();
    let dir = session_profile::session_dir_for(paths, &info.num, &info.email);
    if let Some(creds) = session_profile::read_credentials(keychain, &dir).filter(|c| !c.is_empty())
    {
        if session_profile::identity_drifted(&dir, &info.email, &info.org_uuid) {
            lines.push("session profile: ignored (different account)".into());
        } else {
            lines.extend(label_token_status("session profile", &creds));
        }
    }
    lines.extend(label_token_status("stored backup", &info.creds));
    lines
}

/// Slots that provably authenticate as the same account: one credential
/// lineage in two slots, or one non-empty uuid + org recorded twice.
pub fn duplicate_account_warnings(infos: &[AccountInfo], data: &JsonObject) -> Vec<String> {
    let accounts = store::accounts(data);
    let mut by_fp: Vec<(String, &str)> = Vec::new();
    let mut by_identity: Vec<((String, &str), &str)> = Vec::new();
    let mut out = Vec::new();
    for info in infos {
        let num = info.num.as_str();
        if let Some(fp) = credential_fingerprint(&info.creds) {
            match by_fp.iter().find(|(f, _)| *f == fp) {
                Some((_, other)) => out.push(format!(
                    "Account-{other} and Account-{num} hold the same credential ({}) — one \
                     slot's backup was overwritten. Log in with the missing account and \
                     re-add it: ccswap add --slot N",
                    info.email
                )),
                None => by_fp.push((fp, num)),
            }
        }
        let uuid = accounts
            .and_then(|a| a.get(num))
            .and_then(|a| a.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if uuid.is_empty() {
            continue;
        }
        let key = (uuid, info.org_uuid.as_str());
        match by_identity.iter().find(|(k, _)| *k == key) {
            Some((_, other)) if *other != num => out.push(format!(
                "Account-{other} and Account-{num} both authenticate as {} — remove or \
                 re-login one of them.",
                info.email
            )),
            Some(_) => {}
            None => by_identity.push((key, num)),
        }
    }
    out
}

/// Python equality over the compared fields: `25 == 25.0`.
fn same_field(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Heuristic for two generations of one account: identical 5h and 7d
/// percentages with identical, non-null reset times.
pub fn lockstep_usage_warnings(
    infos: &[AccountInfo],
    entries: &std::collections::HashMap<String, UsageEntry>,
) -> Vec<String> {
    let mut seen: Vec<([Value; 4], &str)> = Vec::new();
    let mut out = Vec::new();
    for info in infos {
        let Some(entry) = entries.get(&info.num) else {
            continue;
        };
        let DecisionValue::Usage(usage) = entry.decision_value() else {
            continue;
        };
        let (Some(h5), Some(d7)) = (
            usage.get("five_hour").and_then(Value::as_object),
            usage.get("seven_day").and_then(Value::as_object),
        ) else {
            continue;
        };
        let field =
            |w: &serde_json::Map<String, Value>, k: &str| w.get(k).cloned().unwrap_or(Value::Null);
        let key = [
            field(h5, "pct"),
            field(h5, "resets_at"),
            field(d7, "pct"),
            field(d7, "resets_at"),
        ];
        if key.iter().any(Value::is_null) {
            continue;
        }
        match seen
            .iter()
            .find(|(k, _)| k.iter().zip(&key).all(|(a, b)| same_field(a, b)))
        {
            Some((_, other)) => out.push(format!(
                "Account-{other} and Account-{} report identical usage and reset times — \
                 they may be the same account (issue #117). If it persists, log in with \
                 the missing account and re-add it: ccswap add --slot N",
                info.num
            )),
            None => seen.push((key, &info.num)),
        }
    }
    out
}
