//! `cache/usage.json` (schemaVersion 2), shared live with Python `cswap`:
//! rows are guarded by the slot's `(email, organizationUuid)` so a reused
//! slot never serves the previous account's row, and writes are fenced by a
//! leased claim id so concurrent collectors never double-fetch or overwrite
//! each other.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::error::Result;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::poll_policy::{
    EDGE_BACKOFF_S, EXHAUSTED_INTERVAL_S, JITTER_FRAC, RECENT_429_WINDOW_S, RESET_SLACK_S,
    SERVE_TTL_S,
};
use crate::settings::atomic_write_json;
use crate::usage::{Sentinel, now_secs, parse_reset_ts, relevant_windows};

pub const SCHEMA_VERSION: i64 = 2;
/// Trusted for switch decisions; older means headroom unknown.
pub const STALE_OK_S: f64 = 300.0;
pub const CLAIM_TTL_S: f64 = 90.0;
/// Lease of rows written by collectors that predate `claimUntil`.
pub const LEGACY_CLAIM_TTL_S: f64 = 10.0;
pub const TRUST_MAX_AGE_S: f64 = 3600.0;
pub const RATE_LIMIT_TRUST_MAX_AGE_S: f64 = 7200.0;
pub const BACKOFF_BASE_S: f64 = 30.0;
pub const BACKOFF_CAP_S: f64 = 600.0;
pub const BACKOFF_MAX_SHIFT: i64 = 32;
pub const RETRY_AFTER_MARGIN_S: f64 = 900.0;
pub const RETRY_AFTER_FLOOR_CAP_S: f64 = 4500.0;
pub const AUTH_DEAD_STRIKES: i64 = 1;
pub const PERMANENT_AUTH_ERRORS: [&str; 2] = ["invalid_grant", "no_refresh_token"];

/// `(email, organizationUuid)`.
pub type Identity = (String, String);

/// What one fetch produced. A sentinel record only releases the claim.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchRecord {
    pub usage: Option<Value>,
    pub error: Option<String>,
    pub retry_after_s: Option<f64>,
    pub sentinel: Option<Sentinel>,
    /// Fingerprint of the generation a permanent auth error condemns.
    pub struck_fp: Option<String>,
}

impl FetchRecord {
    pub fn sentinel(sentinel: Sentinel) -> Self {
        FetchRecord {
            sentinel: Some(sentinel),
            ..Default::default()
        }
    }

    pub fn error(kind: impl Into<String>, retry_after_s: Option<f64>) -> Self {
        FetchRecord {
            error: Some(kind.into()),
            retry_after_s,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageEntry {
    pub sentinel: Option<Sentinel>,
    pub last_good: Option<Value>,
    pub fetched_at: Option<f64>,
    pub age_s: Option<f64>,
    pub last_attempt_at: Option<f64>,
    pub consecutive_failures: i64,
    pub last_error: Option<String>,
    pub backoff_until: Option<f64>,
    pub next_poll_at: Option<f64>,
    pub poll_interval_s: Option<f64>,
    pub last_429_at: Option<f64>,
    pub auth_dead_strikes: i64,
    pub struck_fingerprint: Option<String>,
    /// Older than `STALE_OK_S` but still decision-grade: a refetch is
    /// failing, planned for later, or in flight.
    pub trust_extended: bool,
    pub claim_until: Option<f64>,
}

impl UsageEntry {
    pub fn fresh(&self, now: f64, ttl: f64) -> bool {
        self.fetched_at.is_some_and(|f| now - f <= ttl)
    }

    pub fn in_backoff(&self, now: f64) -> bool {
        self.backoff_until.is_some_and(|b| now < b)
    }

    /// A 429 within the last hour, counted from the end of an honoured
    /// `Retry-After` block when that is later.
    pub fn recent_429(&self, now: f64) -> bool {
        let Some(mut anchor) = self.last_429_at else {
            return false;
        };
        if self.last_error.as_deref() == Some("http-429")
            && let Some(b) = self.backoff_until.filter(|b| *b > anchor)
        {
            anchor = b;
        }
        now < anchor + RECENT_429_WINDOW_S
    }

    pub fn claimed(&self, now: f64) -> bool {
        live_claim(self.claim_until, self.last_attempt_at, now)
    }

    /// Struck dead, unless both sides carry a fingerprint and they differ
    /// (the slot's credential was replaced since).
    pub fn token_dead(&self, stored_fp: Option<&str>) -> bool {
        if self.auth_dead_strikes < AUTH_DEAD_STRIKES {
            return false;
        }
        !matches!((stored_fp, self.struck_fingerprint.as_deref()), (Some(s), Some(t)) if s != t)
    }

    /// The value switch decisions and `--json` may act on.
    pub fn decision_value(&self) -> DecisionValue<'_> {
        if let Some(s) = self.sentinel {
            return DecisionValue::Sentinel(s);
        }
        match (&self.last_good, self.age_s) {
            (Some(usage), Some(age)) if age <= STALE_OK_S || self.trust_extended => {
                DecisionValue::Usage(usage)
            }
            _ => DecisionValue::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecisionValue<'a> {
    Sentinel(Sentinel),
    Usage(&'a Value),
    Unavailable,
}

fn live_claim(claim_until: Option<f64>, last_attempt_at: Option<f64>, now: f64) -> bool {
    match claim_until {
        Some(until) => now < until,
        None => last_attempt_at.is_some_and(|t| now - t < LEGACY_CLAIM_TTL_S),
    }
}

fn plan_oversleeps(next_poll_at: Option<f64>, interval: Option<f64>, now: f64) -> bool {
    let Some(next) = next_poll_at else {
        return false;
    };
    let interval = interval
        .filter(|i| *i != 0.0)
        .unwrap_or(EXHAUSTED_INTERVAL_S)
        .max(EXHAUSTED_INTERVAL_S);
    next > now + interval * (1.0 + JITTER_FRAC) + RESET_SLACK_S
}

/// A stored plan that waits implausibly long (a reset-parked plan from an
/// older release).
pub fn plan_oversleeps_interval(entry: &UsageEntry, now: f64) -> bool {
    plan_oversleeps(entry.next_poll_at, entry.poll_interval_s, now)
}

/// The candidate most in need of a fetch: never-fetched first, then the
/// oldest; ties by slot.
pub fn due_candidate(
    candidates: &[String],
    entries: &HashMap<String, UsageEntry>,
    now: f64,
) -> Option<String> {
    let mut due: Vec<(u8, f64, &String)> = Vec::new();
    for num in candidates {
        let Some(entry) = entries.get(num) else {
            due.push((0, 0.0, num));
            continue;
        };
        if entry.sentinel.is_some() || entry.token_dead(None) || entry.in_backoff(now) {
            continue;
        }
        if entry.next_poll_at.is_some_and(|n| now < n) && !plan_oversleeps_interval(entry, now) {
            continue;
        }
        match entry.fetched_at {
            None => due.push((0, 0.0, num)),
            Some(f) => due.push((1, f, num)),
        }
    }
    due.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then_with(|| a.2.cmp(b.2))
    });
    due.first().map(|d| d.2.clone())
}

fn earliest_reset(last_good: Option<&Value>, models: &[String]) -> Option<f64> {
    relevant_windows(last_good, models)
        .iter()
        .filter_map(|w| parse_reset_ts(w.resets_at.as_ref()))
        .reduce(f64::min)
}

/// A 429'd measurement stays trustworthy until its first window resets,
/// and never past `RATE_LIMIT_TRUST_MAX_AGE_S`.
fn rate_limited_trust_ok(
    last_good: Option<&Value>,
    age_s: Option<f64>,
    now: f64,
    models: &[String],
) -> bool {
    let Some(age) = age_s else {
        return false;
    };
    let ceiling = now + (RATE_LIMIT_TRUST_MAX_AGE_S - age);
    let bound = earliest_reset(last_good, models).map_or(ceiling, |s| s.min(ceiling));
    now < bound
}

pub fn failure_backoff_s(
    consecutive_failures: i64,
    retry_after_s: Option<f64>,
    rate_limited: bool,
) -> f64 {
    let shift = (consecutive_failures - 1).clamp(0, BACKOFF_MAX_SHIFT);
    let computed = (BACKOFF_BASE_S * 2f64.powi(shift as i32)).min(BACKOFF_CAP_S);
    let Some(retry_after) = retry_after_s else {
        return computed;
    };
    if retry_after == 0.0 {
        if !rate_limited {
            return computed;
        }
        return computed.clamp(EDGE_BACKOFF_S, BACKOFF_CAP_S);
    }
    let mut asked = retry_after;
    if retry_after > BACKOFF_CAP_S && rate_limited {
        asked = retry_after + RETRY_AFTER_MARGIN_S;
    }
    asked = asked.min(if rate_limited {
        RETRY_AFTER_FLOOR_CAP_S
    } else {
        TRUST_MAX_AGE_S
    });
    asked.max(computed)
}

type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

#[derive(Clone)]
pub struct UsageStore {
    path: PathBuf,
    lock_path: PathBuf,
    clock: Clock,
}

impl UsageStore {
    pub fn new(cache_dir: &Path) -> Self {
        Self::with_clock(cache_dir, Arc::new(now_secs))
    }

    pub fn with_clock(cache_dir: &Path, clock: Clock) -> Self {
        UsageStore {
            path: cache_dir.join("usage.json"),
            lock_path: cache_dir.join(".usage.lock"),
            clock,
        }
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    /// One entry per requested slot; a missing or other-identity row reads
    /// as empty.
    pub fn entries(
        &self,
        identities: &[(String, Identity)],
        models: &[String],
    ) -> HashMap<String, UsageEntry> {
        let now = self.now();
        let rows = self.read_rows();
        identities
            .iter()
            .map(|(num, identity)| {
                let entry = match rows.get(num).and_then(Value::as_object) {
                    Some(row) if matches(row, identity) => entry_from_row(row, now, models),
                    _ => UsageEntry::default(),
                };
                (num.clone(), entry)
            })
            .collect()
    }

    /// Claims every listed slot unconditionally.
    pub fn claim(
        &self,
        nums: &[String],
        identities: &[(String, Identity)],
    ) -> Result<HashMap<String, String>> {
        if nums.is_empty() {
            return Ok(HashMap::new());
        }
        let now = self.now();
        let claims: HashMap<String, String> =
            nums.iter().map(|n| (n.clone(), new_claim_id())).collect();
        self.mutate_nums(identities, nums, |num, row| {
            row.insert("lastAttemptAt".into(), json!(now));
            row.insert("claimId".into(), claims[num].clone().into());
            row.insert("claimUntil".into(), json!(now + CLAIM_TTL_S));
        })?;
        Ok(claims)
    }

    /// Atomically claims the slots that are due, in request order.
    pub fn reserve(
        &self,
        nums: &[String],
        identities: &[(String, Identity)],
        respect_plans: bool,
        repair_overslept: bool,
    ) -> Result<Vec<(String, String)>> {
        if nums.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.now();
        let _lock = StoreLock::acquire(&self.lock_path, STORE_LOCK_TIMEOUT)?;
        let mut rows = self.read_rows();
        let mut won = Vec::new();
        for num in nums {
            let identity = identity_of(identities, num);
            let fresh = match rows.get(num).and_then(Value::as_object) {
                Some(row) if matches(row, identity) => {
                    if !row_eligible(row, now, respect_plans, repair_overslept) {
                        continue;
                    }
                    false
                }
                _ => true,
            };
            if fresh {
                rows.insert(num.clone(), fresh_row(identity));
            }
            let row = rows[num].as_object_mut().expect("row is an object");
            let claim_id = new_claim_id();
            row.insert("lastAttemptAt".into(), json!(now));
            row.insert("claimId".into(), claim_id.clone().into());
            row.insert("claimUntil".into(), json!(now + CLAIM_TTL_S));
            won.push((num.clone(), claim_id));
        }
        if !won.is_empty() {
            self.write_rows(rows)?;
        }
        Ok(won)
    }

    /// Commits fetch outcomes. With `claims`, only rows still holding that
    /// claim for the same identity are written; without, a live lease held
    /// by someone else wins. Returns the slots written.
    pub fn record(
        &self,
        outcomes: &[(String, FetchRecord)],
        identities: &[(String, Identity)],
        claims: Option<&HashMap<String, String>>,
        plans: &HashMap<String, (f64, f64)>,
    ) -> Result<HashSet<String>> {
        let mut accepted = HashSet::new();
        if outcomes.is_empty() {
            return Ok(accepted);
        }
        let now = self.now();
        let _lock = StoreLock::acquire(&self.lock_path, STORE_LOCK_TIMEOUT)?;
        let mut rows = self.read_rows();
        for (num, rec) in outcomes {
            let identity = identity_of(identities, num);
            let row = rows.get(num).and_then(Value::as_object);
            if let Some(claims) = claims {
                let held = claims.get(num).is_some_and(|expected| {
                    row.is_some_and(|r| {
                        matches(r, identity)
                            && r.get("claimId").and_then(Value::as_str) == Some(expected)
                    })
                });
                if !held {
                    continue;
                }
            } else if row.is_some_and(|r| {
                !r.get("claimId").is_none_or(Value::is_null)
                    && now < num_or_none(r.get("claimUntil")).unwrap_or(0.0)
            }) {
                continue;
            } else if !row.is_some_and(|r| matches(r, identity)) {
                rows.insert(num.clone(), fresh_row(identity));
            }
            let row = rows[num].as_object_mut().expect("row is an object");
            apply_outcome(row, rec, plans.get(num), now);
            accepted.insert(num.clone());
        }
        if !accepted.is_empty() {
            self.write_rows(rows)?;
        }
        Ok(accepted)
    }

    pub fn set_poll_plan(
        &self,
        plans: &[(String, (f64, f64))],
        identities: &[(String, Identity)],
    ) -> Result<()> {
        let nums: Vec<String> = plans.iter().map(|(n, _)| n.clone()).collect();
        self.mutate_nums(identities, &nums, |num, row| {
            let (next, interval) = plans.iter().find(|(n, _)| n == num).expect("planned").1;
            row.insert("nextPollAt".into(), json!(next));
            row.insert("pollIntervalS".into(), json!(interval));
        })
    }

    /// Pulls a just-activated slot's plan to the active floor, anchored on
    /// its last measurement; only ever earlier. Best-effort: the switch it
    /// follows has already committed.
    pub fn replan_new_active(&self, num: &str, identity: &Identity) {
        let identities = [(num.to_string(), identity.clone())];
        let now = self.now();
        let Some(entry) = self.entries(&identities, &[]).remove(num) else {
            return;
        };
        let Some(fetched_at) = entry.fetched_at else {
            return;
        };
        let next = now.max(fetched_at + crate::poll_policy::MIN_INTERVAL_S);
        if entry.next_poll_at.is_some_and(|n| n <= next) {
            return;
        }
        let _ = self.set_poll_plan(
            &[(num.to_string(), (next, crate::poll_policy::MIN_INTERVAL_S))],
            &identities,
        );
    }

    /// Lift the dead-token quarantine after a slot's credential was replaced.
    pub fn clear_dead_token(&self, slots: &[(String, Identity)]) -> Result<()> {
        let nums: Vec<String> = slots.iter().map(|(n, _)| n.clone()).collect();
        self.mutate_nums(slots, &nums, |_, row| {
            row.insert("claimId".into(), Value::Null);
            row.insert("claimUntil".into(), json!(0.0));
            row.insert("authDeadStrikes".into(), json!(0));
            row.insert("struckFingerprint".into(), Value::Null);
            row.insert("consecutiveFailures".into(), json!(0));
            row.insert("lastError".into(), Value::Null);
            row.insert("backoffUntil".into(), Value::Null);
        })
    }

    /// The slot's dead-token verdict as stored; blank when the row is missing
    /// or belongs to another identity.
    pub fn dead_token_strike(&self, num: &str, identity: &Identity) -> DeadTokenStrike {
        let rows = self.read_rows();
        let Some(row) = rows
            .get(num)
            .and_then(Value::as_object)
            .filter(|r| matches(r, identity))
        else {
            return DeadTokenStrike::default();
        };
        DeadTokenStrike {
            strikes: py_int(row.get("authDeadStrikes")),
            fingerprint: row
                .get("struckFingerprint")
                .filter(|v| !v.is_null())
                .cloned(),
        }
    }

    fn mutate_nums(
        &self,
        identities: &[(String, Identity)],
        nums: &[String],
        apply: impl Fn(&str, &mut Map<String, Value>),
    ) -> Result<()> {
        if nums.is_empty() {
            return Ok(());
        }
        let _lock = StoreLock::acquire(&self.lock_path, STORE_LOCK_TIMEOUT)?;
        let mut rows = self.read_rows();
        for num in nums {
            let identity = identity_of(identities, num);
            if !rows
                .get(num)
                .and_then(Value::as_object)
                .is_some_and(|r| matches(r, identity))
            {
                rows.insert(num.clone(), fresh_row(identity));
            }
            apply(num, rows[num].as_object_mut().expect("row is an object"));
        }
        self.write_rows(rows)
    }

    fn write_rows(&self, rows: Map<String, Value>) -> Result<()> {
        let data = json!({"schemaVersion": SCHEMA_VERSION, "accounts": Value::Object(rows)});
        atomic_write_json(&self.path, &data)
    }

    /// Unreadable, invalid, legacy or future-schema files read as empty.
    fn read_rows(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(mut raw))
                if raw.get("schemaVersion").and_then(Value::as_f64)
                    == Some(SCHEMA_VERSION as f64) =>
            {
                match raw.remove("accounts") {
                    Some(Value::Object(rows)) => rows,
                    _ => Map::new(),
                }
            }
            _ => Map::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeadTokenStrike {
    pub strikes: i64,
    /// The condemned generation's fingerprint; `None` for rows struck before
    /// fingerprints were recorded, which bind unconditionally.
    pub fingerprint: Option<Value>,
}

impl DeadTokenStrike {
    /// A strike condemns the generation that was POSTed: a different stored
    /// fingerprint means the credential was replaced since and the strike is healed.
    pub fn token_dead(&self, stored_fp: Option<&str>) -> bool {
        if self.strikes < AUTH_DEAD_STRIKES {
            return false;
        }
        match (stored_fp, &self.fingerprint) {
            (Some(stored), Some(struck)) => struck.as_str() == Some(stored),
            _ => true,
        }
    }
}

fn identity_of<'a>(identities: &'a [(String, Identity)], num: &str) -> &'a Identity {
    &identities
        .iter()
        .find(|(n, _)| n == num)
        .expect("every slot has an identity")
        .1
}

fn apply_outcome(
    row: &mut Map<String, Value>,
    rec: &FetchRecord,
    plan: Option<&(f64, f64)>,
    now: f64,
) {
    row.insert("claimId".into(), Value::Null);
    row.insert("claimUntil".into(), json!(0.0));
    if rec.sentinel.is_some() {
        return;
    }
    row.insert("lastAttemptAt".into(), json!(now));
    match &rec.error {
        None => {
            row.insert("lastGood".into(), rec.usage.clone().unwrap_or(Value::Null));
            row.insert("fetchedAt".into(), json!(now));
            if let Some((next, interval)) = plan {
                row.insert("nextPollAt".into(), json!(next));
                row.insert("pollIntervalS".into(), json!(interval));
            }
            row.insert("consecutiveFailures".into(), json!(0));
            row.insert("lastError".into(), Value::Null);
            row.insert("backoffUntil".into(), Value::Null);
            row.insert("authDeadStrikes".into(), json!(0));
        }
        Some(error) => {
            let failures = py_int(row.get("consecutiveFailures")) + 1;
            row.insert("consecutiveFailures".into(), json!(failures));
            row.insert("lastError".into(), error.as_str().into());
            let rate_limited = error == "http-429";
            if rate_limited {
                row.insert("last429At".into(), json!(now));
            }
            row.insert(
                "backoffUntil".into(),
                json!(now + failure_backoff_s(failures, rec.retry_after_s, rate_limited)),
            );
            if PERMANENT_AUTH_ERRORS.contains(&error.as_str()) {
                let strikes = py_int(row.get("authDeadStrikes")) + 1;
                row.insert("authDeadStrikes".into(), json!(strikes));
                row.insert(
                    "struckFingerprint".into(),
                    rec.struck_fp.clone().map_or(Value::Null, Value::from),
                );
            }
        }
    }
}

fn entry_from_row(row: &Map<String, Value>, now: f64, models: &[String]) -> UsageEntry {
    let fetched_at = num_or_none(row.get("fetchedAt"));
    let last_good = row.get("lastGood").filter(|v| v.is_object()).cloned();
    let age_s = fetched_at.map(|f| now - f);
    let consecutive_failures = py_int(row.get("consecutiveFailures"));
    let next_poll_at = num_or_none(row.get("nextPollAt"));
    let last_attempt_at = num_or_none(row.get("lastAttemptAt"));
    let claim_until = num_or_none(row.get("claimUntil"));
    let last_error = text_or_none(row.get("lastError"));
    let within_ceiling = if last_error.as_deref() == Some("http-429") {
        rate_limited_trust_ok(last_good.as_ref(), age_s, now, models)
    } else {
        age_s.is_some_and(|a| a <= TRUST_MAX_AGE_S)
    };
    let trust_extended = within_ceiling
        && (consecutive_failures > 0
            || next_poll_at.is_some_and(|n| now < n)
            || live_claim(claim_until, last_attempt_at, now));
    UsageEntry {
        sentinel: None,
        last_good,
        fetched_at,
        age_s,
        last_attempt_at,
        consecutive_failures,
        last_error,
        backoff_until: num_or_none(row.get("backoffUntil")),
        next_poll_at,
        poll_interval_s: num_or_none(row.get("pollIntervalS")),
        last_429_at: num_or_none(row.get("last429At")),
        auth_dead_strikes: py_int(row.get("authDeadStrikes")),
        struck_fingerprint: text_or_none(row.get("struckFingerprint")),
        trust_extended,
        claim_until,
    }
}

fn row_eligible(
    row: &Map<String, Value>,
    now: f64,
    respect_plans: bool,
    repair_overslept: bool,
) -> bool {
    if py_int(row.get("authDeadStrikes")) >= AUTH_DEAD_STRIKES {
        return false;
    }
    if num_or_none(row.get("backoffUntil")).is_some_and(|b| now < b) {
        return false;
    }
    if live_claim(
        num_or_none(row.get("claimUntil")),
        num_or_none(row.get("lastAttemptAt")),
        now,
    ) {
        return false;
    }
    let stale = num_or_none(row.get("fetchedAt")).is_none_or(|f| now - f > SERVE_TTL_S);
    let next_poll_at = num_or_none(row.get("nextPollAt"));
    let poll_due = next_poll_at.is_some_and(|n| now >= n);
    let overslept = repair_overslept
        && plan_oversleeps(next_poll_at, num_or_none(row.get("pollIntervalS")), now);
    if respect_plans {
        return stale && (poll_due || next_poll_at.is_none() || overslept);
    }
    if repair_overslept {
        return poll_due || (stale && (next_poll_at.is_none() || overslept));
    }
    poll_due || stale
}

fn fresh_row(identity: &Identity) -> Value {
    json!({"email": identity.0, "organizationUuid": identity.1})
}

fn matches(row: &Map<String, Value>, identity: &Identity) -> bool {
    row.get("email").and_then(Value::as_str) == Some(identity.0.as_str())
        && row.get("organizationUuid").map_or(Some(""), Value::as_str) == Some(identity.1.as_str())
}

fn num_or_none(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        other => other.as_f64(),
    }
}

/// `int(x or 0)` for what a row can hold.
fn py_int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::Bool(b)) => i64::from(*b),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text_or_none(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// `uuid.uuid4().hex`.
fn new_claim_id() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        let seed = now_secs().to_bits() ^ u64::from(std::process::id());
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_usage_store.py (FakeClock starting at
    //! 1_000_000.0, IDENT = {"1": ("a@x.com", ""), "2": ("b@x.com", "org-2")}).
    use super::*;
    use std::sync::Mutex;

    const START: f64 = 1_000_000.0;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: UsageStore,
        clock: Arc<Mutex<f64>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let clock = Arc::new(Mutex::new(START));
            let c = clock.clone();
            let store = UsageStore::with_clock(dir.path(), Arc::new(move || *c.lock().unwrap()));
            Fixture {
                _dir: dir,
                store,
                clock,
            }
        }

        fn advance(&self, secs: f64) {
            *self.clock.lock().unwrap() += secs;
        }

        fn raw(&self) -> Value {
            serde_json::from_str(&std::fs::read_to_string(&self.store.path).unwrap()).unwrap()
        }

        fn write_raw(&self, v: &Value) {
            std::fs::write(&self.store.path, v.to_string()).unwrap();
        }

        fn entry(&self, num: &str) -> UsageEntry {
            self.store.entries(&ident(), &[]).remove(num).unwrap()
        }

        fn ok(&self, num: &str, usage: Value) {
            self.store
                .record(
                    &[(
                        num.into(),
                        FetchRecord {
                            usage: Some(usage),
                            ..Default::default()
                        },
                    )],
                    &ident(),
                    None,
                    &HashMap::new(),
                )
                .unwrap();
        }

        fn fail(&self, num: &str, error: &str, retry_after: Option<f64>) {
            self.store
                .record(
                    &[(num.into(), FetchRecord::error(error, retry_after))],
                    &ident(),
                    None,
                    &HashMap::new(),
                )
                .unwrap();
        }

        fn reserve(&self, nums: &[&str]) -> Vec<String> {
            let nums: Vec<String> = nums.iter().map(|s| s.to_string()).collect();
            self.store
                .reserve(&nums, &ident(), true, true)
                .unwrap()
                .into_iter()
                .map(|(n, _)| n)
                .collect()
        }
    }

    fn ident() -> Vec<(String, Identity)> {
        vec![
            ("1".into(), ("a@x.com".into(), "".into())),
            ("2".into(), ("b@x.com".into(), "org-2".into())),
        ]
    }

    fn u(pct: f64) -> Value {
        json!({"five_hour": {"pct": pct}})
    }

    #[test]
    fn schema_round_trip_and_key_order() {
        let f = Fixture::new();
        f.ok("1", u(25.0));
        let raw = f.raw();
        assert_eq!(raw["schemaVersion"], 2);
        let keys: Vec<&String> = raw["accounts"]["1"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "email",
                "organizationUuid",
                "claimId",
                "claimUntil",
                "lastAttemptAt",
                "lastGood",
                "fetchedAt",
                "consecutiveFailures",
                "lastError",
                "backoffUntil",
                "authDeadStrikes"
            ]
        );
        assert_eq!(f.entry("1").last_good, Some(u(25.0)));
        assert_eq!(f.entry("1").fetched_at, Some(START));
    }

    #[test]
    fn legacy_and_corrupt_files_read_as_empty() {
        let f = Fixture::new();
        for raw in [
            json!({"timestamp": 1, "data": {}}),
            json!({"schemaVersion": 3, "accounts": {"1": {"email": "a@x.com"}}}),
            json!({"schemaVersion": 2, "accounts": []}),
            json!([1]),
        ] {
            f.write_raw(&raw);
            assert_eq!(f.entry("1"), UsageEntry::default());
        }
        std::fs::write(&f.store.path, "{not json").unwrap();
        assert_eq!(f.entry("1"), UsageEntry::default());
    }

    #[test]
    fn failure_keeps_last_good_and_backs_off() {
        let f = Fixture::new();
        f.ok("1", u(25.0));
        f.advance(400.0);
        f.fail("1", "timeout", None);
        let e = f.entry("1");
        assert_eq!(e.last_good, Some(u(25.0)));
        assert_eq!(e.fetched_at, Some(START));
        assert_eq!(e.consecutive_failures, 1);
        assert_eq!(e.last_error.as_deref(), Some("timeout"));
        assert_eq!(e.backoff_until, Some(START + 400.0 + 30.0));
        assert!(e.trust_extended);
        assert_eq!(e.decision_value(), DecisionValue::Usage(&u(25.0)));
        f.advance(5.0);
        f.fail("1", "timeout", None);
        assert_eq!(f.entry("1").backoff_until, Some(START + 405.0 + 60.0));
        f.ok("1", u(30.0));
        let e = f.entry("1");
        assert_eq!(
            (e.consecutive_failures, e.last_error, e.backoff_until),
            (0, None, None)
        );
    }

    #[test]
    fn trust_ends_at_the_hour_ceiling() {
        let f = Fixture::new();
        f.ok("1", u(25.0));
        f.advance(400.0);
        assert_eq!(f.entry("1").decision_value(), DecisionValue::Unavailable);
        f.fail("1", "network", None);
        f.advance(3200.0);
        assert!(f.entry("1").trust_extended);
        f.advance(1.0);
        assert_eq!(f.entry("1").decision_value(), DecisionValue::Unavailable);
    }

    #[test]
    fn rate_limited_trust_ends_at_the_earliest_reset_or_two_hours() {
        let f = Fixture::new();
        let reset = chrono::DateTime::from_timestamp(START as i64 + 5000, 0)
            .unwrap()
            .to_rfc3339();
        f.ok("1", json!({"five_hour": {"pct": 50.0, "resets_at": reset}}));
        f.fail("1", "http-429", None);
        f.advance(4999.0);
        assert!(f.entry("1").trust_extended);
        f.advance(1.0);
        assert!(!f.entry("1").trust_extended);
        f.ok("2", u(5.0));
        f.fail("2", "http-429", None);
        f.advance(7199.0);
        assert!(f.entry("2").trust_extended);
        f.advance(1.0);
        assert!(!f.entry("2").trust_extended);
    }

    #[test]
    fn backoff_schedule() {
        assert_eq!(failure_backoff_s(1, None, false), 30.0);
        assert_eq!(failure_backoff_s(2, None, false), 60.0);
        assert_eq!(failure_backoff_s(5, None, false), 480.0);
        assert_eq!(failure_backoff_s(6, None, false), 600.0);
        assert_eq!(failure_backoff_s(0, None, false), 30.0);
        assert_eq!(failure_backoff_s(10_000, None, true), 600.0);
        assert_eq!(failure_backoff_s(1, Some(120.0), true), 120.0);
        assert_eq!(failure_backoff_s(3, Some(10.0), true), 120.0);
        assert_eq!(failure_backoff_s(1, Some(0.0), true), 300.0);
        assert_eq!(failure_backoff_s(1, Some(0.0), false), 30.0);
        assert_eq!(failure_backoff_s(1, Some(3000.0), true), 3900.0);
        assert_eq!(failure_backoff_s(1, Some(4000.0), true), 4500.0);
        assert_eq!(failure_backoff_s(1, Some(9000.0), false), 3600.0);
        assert_eq!(failure_backoff_s(1, Some(700.0), false), 700.0);
    }

    #[test]
    fn retry_after_seconds_drive_the_stored_backoff() {
        let f = Fixture::new();
        f.fail("1", "http-429", Some(120.0));
        let e = f.entry("1");
        assert_eq!(e.backoff_until, Some(START + 120.0));
        assert_eq!(e.last_429_at, Some(START));
        assert!(e.recent_429(START + 120.0 + 3599.0));
        assert!(!e.recent_429(START + 120.0 + 3600.0));
    }

    fn iso_at(ts: f64) -> String {
        chrono::DateTime::from_timestamp(ts as i64, 0)
            .unwrap()
            .to_rfc3339()
    }

    fn fable() -> Vec<String> {
        vec!["Fable".into()]
    }

    fn backoff_until(f: &Fixture, models: &[String]) -> f64 {
        f.store.entries(&ident(), models)["1"]
            .backoff_until
            .unwrap()
    }

    #[test]
    fn a_non_429_ask_is_bounded_at_its_own_trust_ceiling() {
        for ask in [601.0, 3600.0] {
            assert_eq!(failure_backoff_s(1, Some(ask), false), ask);
        }
        for ask in [
            3601.0,
            4500.0,
            7200.0,
            10_000.0,
            20_000.0,
            86_400.0,
            f64::INFINITY,
        ] {
            assert_eq!(
                failure_backoff_s(1, Some(ask), false),
                TRUST_MAX_AGE_S,
                "ask={ask}"
            );
        }
        let overflow: f64 = "1e400".parse().unwrap();
        assert_eq!(failure_backoff_s(1, Some(overflow), false), TRUST_MAX_AGE_S);
        assert_eq!(failure_backoff_s(1, Some(3600.0), true), 4500.0);
        assert_eq!(failure_backoff_s(1, Some(f64::INFINITY), true), 4500.0);
    }

    #[test]
    fn shortening_a_429_wait_cannot_move_when_the_row_goes_unknown() {
        let decision_at = |stride: f64, checkpoint: f64| {
            let f = Fixture::new();
            f.ok(
                "1",
                json!({"five_hour": {"pct": 50.0, "resets_at": iso_at(START + 1800.0)}}),
            );
            let mut elapsed = 0.0;
            while elapsed < checkpoint {
                f.fail("1", "http-429", Some(3600.0));
                let step = stride.min(checkpoint - elapsed);
                f.advance(step);
                elapsed += step;
            }
            assert_eq!(f.store.now() - START, checkpoint);
            f.entry("1").decision_value() == DecisionValue::Unavailable
        };
        for (checkpoint, expect_unknown) in [(1799.0, false), (1801.0, true)] {
            let hammered = decision_at(37.0, checkpoint);
            let honored = decision_at(1801.0, checkpoint);
            assert_eq!(
                (hammered, honored),
                (expect_unknown, expect_unknown),
                "at +{checkpoint}s"
            );
        }
    }

    #[test]
    fn a_re_block_chain_spends_one_request_per_block() {
        let f = Fixture::new();
        f.ok("1", u(50.0));
        let mut polls = 0;
        for _ in 0..4 {
            polls += 1;
            f.fail("1", "http-429", Some(3600.0));
            let until = backoff_until(&f, &[]);
            f.advance(until - f.store.now());
        }
        assert_eq!(polls, 4);
        assert!(f.store.now() - START >= 4.0 * 3600.0);
    }

    #[test]
    fn the_margin_is_not_traded_away_for_a_dead_scoped_window() {
        let f = Fixture::new();
        f.ok(
            "1",
            json!({
                "five_hour": {"pct": 50.0, "resets_at": iso_at(START + 14_400.0)},
                "seven_day": {"pct": 10.0, "resets_at": iso_at(START + 400_000.0)},
                "scoped": [{"name": "Fable", "pct": 60.0, "resets_at": iso_at(START + 1800.0)}],
            }),
        );
        f.fail("1", "http-429", Some(3600.0));
        assert_eq!(backoff_until(&f, &fable()) - START, 4500.0);
    }

    #[test]
    fn an_expired_trust_does_not_turn_one_block_into_a_request_storm() {
        let block_s = 3600.0;
        let f = Fixture::new();
        f.ok(
            "1",
            json!({
                "five_hour": {"pct": 50.0, "resets_at": iso_at(START + 1800.0)},
                "seven_day": {"pct": 10.0, "resets_at": iso_at(START + 400_000.0)},
            }),
        );
        let mut requests = 0;
        while f.store.now() - START < block_s && requests < 40 {
            requests += 1;
            let remaining = block_s - (f.store.now() - START);
            f.fail("1", "http-429", Some(remaining));
            let until = backoff_until(&f, &fable());
            f.advance(until - f.store.now());
        }
        assert!(requests <= 2, "{requests} requests inside one block");
        assert!(f.store.now() - START >= block_s + RETRY_AFTER_MARGIN_S);
        assert_eq!(
            failure_backoff_s(1, Some(4000.0), true),
            RETRY_AFTER_FLOOR_CAP_S
        );
    }

    #[test]
    fn the_trim_never_lands_inside_the_re_block_band() {
        let measured_band_s = 900.0;
        let f = Fixture::new();
        f.ok(
            "1",
            json!({
                "five_hour": {"pct": 50.0, "resets_at": iso_at(START + 7200.0)},
                "seven_day": {"pct": 10.0, "resets_at": iso_at(START + 30.0 * 86_400.0)},
                "scoped": [{"name": "Fable", "pct": 60.0, "resets_at": iso_at(START + 4000.0)}],
            }),
        );
        f.fail("1", "http-429", Some(3600.0));
        assert!(backoff_until(&f, &fable()) - START >= 3600.0 + measured_band_s);
    }

    #[test]
    fn a_re_block_chain_does_not_shorten_its_own_waits() {
        let f = Fixture::new();
        f.ok(
            "1",
            json!({
                "five_hour": {"pct": 50.0, "resets_at": iso_at(START + 16_000.0)},
                "seven_day": {"pct": 0.0},
            }),
        );
        for block in 0..4 {
            f.fail("1", "http-429", Some(3600.0));
            let waited = backoff_until(&f, &[]) - f.store.now();
            assert_eq!(waited, 4500.0, "block {block}");
            f.advance(waited);
        }
    }

    #[test]
    fn a_non_429_recorded_through_record_does_not_take_the_margin() {
        let f = Fixture::new();
        f.ok("1", u(50.0));
        f.advance(120.0);
        assert_ne!(f.entry("1").decision_value(), DecisionValue::Unavailable);
        let t1 = f.store.now();
        let ask = (TRUST_MAX_AGE_S + RETRY_AFTER_FLOOR_CAP_S) / 2.0;
        f.fail("1", "http-503", Some(ask));
        assert_eq!(backoff_until(&f, &[]) - t1, TRUST_MAX_AGE_S);
    }

    #[test]
    fn identity_guard_hides_and_replaces_other_accounts_rows() {
        let f = Fixture::new();
        f.write_raw(&json!({"schemaVersion": 2, "accounts": {
            "1": {"email": "old@x.com", "organizationUuid": "", "lastGood": u(99.0), "fetchedAt": START},
            "2": {"email": "b@x.com", "lastGood": u(1.0), "fetchedAt": START},
        }}));
        assert_eq!(f.entry("1"), UsageEntry::default());
        assert_eq!(f.entry("2"), UsageEntry::default());
        f.ok("1", u(5.0));
        assert_eq!(f.raw()["accounts"]["1"]["email"], "a@x.com");
        assert_eq!(f.entry("1").last_good, Some(u(5.0)));
    }

    #[test]
    fn reserve_skips_fresh_backoff_claimed_and_dead_rows() {
        let f = Fixture::new();
        assert_eq!(f.reserve(&["1", "2"]), ["1", "2"]);
        assert!(f.reserve(&["1", "2"]).is_empty());
        let raw = f.raw();
        assert_eq!(raw["accounts"]["1"]["claimUntil"], START + 90.0);
        assert_eq!(raw["accounts"]["1"]["claimId"].as_str().unwrap().len(), 32);
        f.advance(91.0);
        f.ok("1", u(1.0));
        f.advance(100.0);
        assert!(f.reserve(&["1"]).is_empty(), "fresh row is served");
        f.advance(81.0);
        assert_eq!(f.reserve(&["1"]), ["1"]);
        f.advance(91.0);
        f.fail("1", "invalid_grant", None);
        f.advance(10_000.0);
        assert!(f.reserve(&["1"]).is_empty(), "dead rows never fetch");
    }

    #[test]
    fn reserve_respects_future_plans_but_repairs_overslept_ones() {
        let f = Fixture::new();
        f.ok("1", u(1.0));
        f.store
            .set_poll_plan(&[("1".into(), (START + 600.0, 600.0))], &ident())
            .unwrap();
        f.advance(400.0);
        assert!(f.reserve(&["1"]).is_empty());
        f.store
            .set_poll_plan(&[("1".into(), (START + 400.0 + 5000.0, 600.0))], &ident())
            .unwrap();
        assert_eq!(f.reserve(&["1"]), ["1"]);
    }

    #[test]
    fn fenced_record_drops_outcomes_whose_claim_was_superseded() {
        let f = Fixture::new();
        let nums = vec!["1".to_string()];
        let first: HashMap<_, _> = f
            .store
            .reserve(&nums, &ident(), false, false)
            .unwrap()
            .into_iter()
            .collect();
        f.advance(91.0);
        let second: HashMap<_, _> = f
            .store
            .reserve(&nums, &ident(), false, false)
            .unwrap()
            .into_iter()
            .collect();
        let rec = |pct| {
            vec![(
                "1".to_string(),
                FetchRecord {
                    usage: Some(u(pct)),
                    ..Default::default()
                },
            )]
        };
        let accepted = f
            .store
            .record(&rec(10.0), &ident(), Some(&first), &HashMap::new())
            .unwrap();
        assert!(accepted.is_empty());
        let accepted = f
            .store
            .record(&rec(20.0), &ident(), Some(&second), &HashMap::new())
            .unwrap();
        assert!(accepted.contains("1"));
        assert_eq!(f.entry("1").last_good, Some(u(20.0)));
        assert_eq!(f.raw()["accounts"]["1"]["claimUntil"], json!(0.0));
    }

    #[test]
    fn unfenced_record_defers_to_a_live_claim() {
        let f = Fixture::new();
        f.reserve(&["1"]);
        f.ok("1", u(10.0));
        assert_eq!(f.entry("1").last_good, None);
        f.advance(91.0);
        f.ok("1", u(10.0));
        assert_eq!(f.entry("1").last_good, Some(u(10.0)));
    }

    #[test]
    fn sentinel_record_only_releases_the_claim() {
        let f = Fixture::new();
        let nums = vec!["1".to_string()];
        let claims: HashMap<_, _> = f
            .store
            .reserve(&nums, &ident(), true, true)
            .unwrap()
            .into_iter()
            .collect();
        f.store
            .record(
                &[("1".into(), FetchRecord::sentinel(Sentinel::TokenExpired))],
                &ident(),
                Some(&claims),
                &HashMap::new(),
            )
            .unwrap();
        let row = &f.raw()["accounts"]["1"];
        assert_eq!(row["claimId"], Value::Null);
        assert!(row.get("lastError").is_none());
        assert!(row.get("fetchedAt").is_none());
    }

    #[test]
    fn permanent_auth_errors_strike_and_bind_the_fingerprint() {
        let f = Fixture::new();
        f.store
            .record(
                &[(
                    "1".into(),
                    FetchRecord {
                        error: Some("invalid_grant".into()),
                        struck_fp: Some("sha256:a".into()),
                        ..Default::default()
                    },
                )],
                &ident(),
                None,
                &HashMap::new(),
            )
            .unwrap();
        let e = f.entry("1");
        assert_eq!(e.auth_dead_strikes, 1);
        assert!(e.token_dead(Some("sha256:a")));
        assert!(e.token_dead(None));
        assert!(!e.token_dead(Some("sha256:b")));
        f.store.clear_dead_token(&ident()[..1]).unwrap();
        assert!(!f.entry("1").token_dead(None));
        f.fail("1", "invalid_grant", None);
        assert_eq!(f.raw()["accounts"]["1"]["struckFingerprint"], Value::Null);
        assert!(f.entry("1").token_dead(Some("sha256:b")));
    }

    #[test]
    fn success_clears_strikes_and_commits_the_plan() {
        let f = Fixture::new();
        f.fail("1", "invalid_grant", None);
        let plans = HashMap::from([("1".to_string(), (START + 200.0, 180.0))]);
        f.store
            .record(
                &[(
                    "1".into(),
                    FetchRecord {
                        usage: Some(u(1.0)),
                        ..Default::default()
                    },
                )],
                &ident(),
                None,
                &plans,
            )
            .unwrap();
        let e = f.entry("1");
        assert_eq!(e.auth_dead_strikes, 0);
        assert_eq!(
            (e.next_poll_at, e.poll_interval_s),
            (Some(START + 200.0), Some(180.0))
        );
    }

    #[test]
    fn legacy_rows_without_claim_until_hold_a_ten_second_lease() {
        let f = Fixture::new();
        f.write_raw(&json!({"schemaVersion": 2, "accounts": {
            "1": {"email": "a@x.com", "organizationUuid": "", "lastAttemptAt": START - 5.0},
        }}));
        assert!(f.entry("1").claimed(START));
        assert!(f.reserve(&["1"]).is_empty());
        f.advance(5.0);
        assert_eq!(f.reserve(&["1"]), ["1"]);
    }

    #[test]
    fn replanning_a_new_active_only_pulls_the_plan_earlier() {
        let f = Fixture::new();
        f.store.replan_new_active("1", &ident()[0].1);
        assert!(
            !f.store.path.exists(),
            "never-measured slots stay plan-less"
        );
        f.ok("1", u(1.0));
        f.store
            .set_poll_plan(&[("1".into(), (START + 600.0, 600.0))], &ident())
            .unwrap();
        f.advance(10.0);
        f.store.replan_new_active("1", &ident()[0].1);
        let e = f.entry("1");
        assert_eq!(
            (e.next_poll_at, e.poll_interval_s),
            (Some(START + 180.0), Some(180.0))
        );
        f.advance(1000.0);
        f.store.replan_new_active("1", &ident()[0].1);
        assert_eq!(f.entry("1").next_poll_at, Some(START + 180.0));
    }

    #[test]
    fn due_candidate_prefers_never_fetched_then_oldest() {
        let f = Fixture::new();
        f.ok("1", u(1.0));
        let cands = vec!["1".to_string(), "2".to_string()];
        let entries = f.store.entries(&ident(), &[]);
        assert_eq!(due_candidate(&cands, &entries, START), Some("2".into()));
        let mut entries = entries;
        entries.get_mut("2").unwrap().sentinel = Some(Sentinel::ApiKey);
        assert_eq!(due_candidate(&cands, &entries, START), Some("1".into()));
    }
}
