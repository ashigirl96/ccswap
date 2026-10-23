//! In-process memory of who a credential lineage belongs to (upstream
//! `_probe_verdicts` / `_provenance_warned`), for decisions made under locks
//! where no network may run. Only definitive verdicts are kept, so a partial
//! answer never blocks a later resync. Keys include the slot's full stored
//! identity, so a slot re-created for another account misses rather than
//! inheriting its predecessor's verdict.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use ccswap_core::paths::Paths;

use crate::store;

type Key = (PathBuf, String, String, String, String, String, String);
type WarnKey = (PathBuf, String, String, &'static str);

fn verdicts() -> MutexGuard<'static, HashMap<Key, bool>> {
    static VERDICTS: OnceLock<Mutex<HashMap<Key, bool>>> = OnceLock::new();
    VERDICTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn warned() -> MutexGuard<'static, HashSet<WarnKey>> {
    static WARNED: OnceLock<Mutex<HashSet<WarnKey>>> = OnceLock::new();
    WARNED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Built fresh at every consult so it revalidates the slot's identity.
fn lineage_key(paths: &Paths, num: &str, email: &str, fingerprint: &str) -> Key {
    let data = store::read_sequence(paths)
        .ok()
        .flatten()
        .unwrap_or_default();
    let own = store::slot_identity(&data, num);
    (
        paths.backup_root(),
        num.to_string(),
        email.to_string(),
        own.email,
        own.organization_uuid,
        own.uuid,
        fingerprint.to_string(),
    )
}

/// `Some(true)`: the lineage is the slot's; `Some(false)`: another account's.
pub fn get(paths: &Paths, num: &str, email: &str, fingerprint: &str) -> Option<bool> {
    let key = lineage_key(paths, num, email, fingerprint);
    verdicts().get(&key).copied()
}

pub fn record(paths: &Paths, num: &str, email: &str, fingerprint: &str, owned: bool) {
    let key = lineage_key(paths, num, email, fingerprint);
    verdicts().insert(key, owned);
}

/// `true` the first time a condition is seen for a slot, so the caller
/// warns once per condition rather than once per pass.
pub fn first_warning(paths: &Paths, num: &str, email: &str, condition: &'static str) -> bool {
    warned().insert((
        paths.backup_root(),
        num.to_string(),
        email.to_string(),
        condition,
    ))
}

pub fn clear_warning(paths: &Paths, num: &str, email: &str, condition: &'static str) {
    warned().remove(&(
        paths.backup_root(),
        num.to_string(),
        email.to_string(),
        condition,
    ));
}
