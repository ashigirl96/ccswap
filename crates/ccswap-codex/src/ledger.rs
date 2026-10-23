use std::collections::BTreeMap;
use std::path::Path;

use ccswap_core::error::{Error, Result};
use serde_json::{Map, Value, json};

/// `codex/accounts.json`. Fields this build does not know are kept on rewrite.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    /// The number last made live by ccswap; stored as a string.
    pub active: Option<u32>,
    pub sequence: Vec<u32>,
    pub accounts: BTreeMap<u32, LedgerAccount>,
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LedgerAccount {
    pub email: String,
    /// Lowercased; the identity key.
    pub account_id: String,
    pub plan: String,
    pub alias: Option<String>,
    /// UTC, `YYYY-MM-DDTHH:MM:SSZ`.
    pub added: String,
    pub extra: Map<String, Value>,
}

impl Ledger {
    /// A missing file is an empty ledger.
    pub fn load(path: &Path) -> Result<Ledger> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
            Err(e) => {
                return Err(Error::config(format!(
                    "Could not read {}: {e}",
                    path.display()
                )));
            }
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| Error::config(format!("{} is not valid JSON: {e}", path.display())))?;
        Ok(Ledger::from_json(value))
    }

    fn from_json(value: Value) -> Ledger {
        let Value::Object(mut top) = value else {
            return Ledger::default();
        };
        let number = |v: &Value| match v {
            Value::String(s) => s.trim().parse::<u32>().ok(),
            Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        };
        let active = top.remove("active").as_ref().and_then(number);
        let sequence = match top.remove("sequence") {
            Some(Value::Array(items)) => items.iter().filter_map(number).collect(),
            _ => Vec::new(),
        };
        let accounts = match top.remove("accounts") {
            Some(Value::Object(rows)) => rows
                .into_iter()
                .filter_map(|(key, row)| {
                    let n = key.trim().parse::<u32>().ok()?;
                    let Value::Object(mut row) = row else {
                        return None;
                    };
                    let mut text = |k: &str| match row.remove(k) {
                        Some(Value::String(s)) => Some(s),
                        _ => None,
                    };
                    let account = LedgerAccount {
                        email: text("email").unwrap_or_default(),
                        account_id: text("account_id").unwrap_or_default(),
                        plan: text("plan").unwrap_or_default(),
                        alias: text("alias").filter(|a| !a.is_empty()),
                        added: text("added").unwrap_or_default(),
                        extra: row,
                    };
                    Some((n, account))
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        Ledger {
            active,
            sequence,
            accounts,
            extra: top,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut top = self.extra.clone();
        top.insert("active".into(), self.active.map(|n| n.to_string()).into());
        top.insert("sequence".into(), json!(self.sequence));
        let accounts: Map<String, Value> = self
            .accounts
            .iter()
            .map(|(n, a)| {
                let mut row = a.extra.clone();
                row.insert("email".into(), a.email.clone().into());
                row.insert("account_id".into(), a.account_id.clone().into());
                row.insert("plan".into(), a.plan.clone().into());
                row.insert("alias".into(), a.alias.clone().into());
                row.insert("added".into(), a.added.clone().into());
                (n.to_string(), Value::Object(row))
            })
            .collect();
        top.insert("accounts".into(), Value::Object(accounts));
        Value::Object(top)
    }

    /// Accounts in `sequence` order, then any rows the sequence misses.
    pub fn ordered(&self) -> Vec<(u32, &LedgerAccount)> {
        let mut out: Vec<(u32, &LedgerAccount)> = self
            .sequence
            .iter()
            .filter_map(|n| self.accounts.get(n).map(|a| (*n, a)))
            .collect();
        for (n, a) in &self.accounts {
            if !self.sequence.contains(n) {
                out.push((*n, a));
            }
        }
        out
    }

    pub fn find_by_account_id(&self, account_id: &str) -> Option<u32> {
        let wanted = account_id.trim().to_lowercase();
        self.accounts
            .iter()
            .find(|(_, a)| a.account_id == wanted)
            .map(|(n, _)| *n)
    }

    pub fn next_number(&self) -> u32 {
        self.accounts
            .keys()
            .chain(&self.sequence)
            .max()
            .map_or(1, |n| n + 1)
    }

    pub fn insert(&mut self, number: u32, account: LedgerAccount) {
        self.accounts.insert(number, account);
        if !self.sequence.contains(&number) {
            self.sequence.push(number);
        }
    }
}
