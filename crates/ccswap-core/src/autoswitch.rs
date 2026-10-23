//! Polls usage and switches accounts before they hit their limits.
//!
//! Cooldown, quarantine and the last move persist in `autoswitch_state.json`
//! so cron-driven `auto --once` runs behave like one long-running loop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use crate::anthropic_client::{TokenAccount, credential_fingerprint, extract_oauth_data};
use crate::error::Result;
use crate::poll_policy::{
    self, ESCALATION_MARGIN_PCT, RESET_SLACK_S, binding_pct, limiting_reset_ts,
};
use crate::snapshot::AccountKind;
use crate::store::format_timestamp;
use crate::usage::{Sentinel, relevant_windows};
use crate::usage_store::{DecisionValue, UsageEntry, due_candidate, plan_oversleeps_interval};

pub mod events;
pub mod host;
pub mod rank;
pub mod run_loop;
pub mod settings;
pub mod state;

#[cfg(test)]
mod tests;

pub use events::{AutoSwitchEvent, EventKind, Trigger, pct_label};
pub use host::{EngineHost, SwitchResult, SwitcherHost};
pub use rank::{Decisions, UsageValue};
pub use run_loop::LoopHandle;
pub use settings::{
    AutoStrategy, AutoSwitchSettings, CliOverrides, autoswitch_models, parse_model_names,
};
pub use state::{STATE_FILENAME, STATE_SCHEMA_VERSION, State, StateFile};

use rank::{BarInput, RankInput, Ranked, binding_recovery_ts};

/// Refresh a target whose token expires within this window: twice Claude
/// Code's own 5-minute buffer.
pub const FRESHEN_BUFFER_MS: f64 = 10.0 * 60.0 * 1000.0;
pub const MAX_SLEEP_S: f64 = poll_policy::EXHAUSTED_INTERVAL_S;
pub const NO_RESET_FALLBACK_S: f64 = 300.0;
/// After this long an owned-and-expired active token stops idling the
/// engine (a dead refresh token would look idle forever).
pub const IDLE_HOLD_MAX_S: f64 = 30.0 * 60.0;

/// Systemic freshen refusals, most actionable first: the tick reports one.
const SYSTEMIC: [(&str, &str); 4] = [
    (
        "store-unmirrored",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR is set — unset it or run ccswap from a normal shell",
    ),
    (
        "invalid_client",
        "cswap's OAuth client was rejected — systemic, not this account",
    ),
    (
        "stash-unreadable",
        "a stashed successor is unreadable — unlock the keychain or fix the file, then retry; `ccswap unclaimed` inspects it",
    ),
    (
        "consume-busy",
        "another cswap surface holds the slot — retries next pass",
    ),
];

fn systemic_rank(status: &str) -> Option<usize> {
    SYSTEMIC.iter().position(|(s, _)| *s == status)
}

/// Outcome of one tick; the values are `auto --once`'s exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    Switched = 0,
    Error = 1,
    NoAction = 2,
    /// Wanted to switch but no viable target, or everything is exhausted.
    Blocked = 3,
}

impl TickOutcome {
    pub fn exit_code(self) -> i32 {
        self as i32
    }
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
pub type Rng<'a> = Box<dyn FnMut() -> f64 + 'a>;
pub type EventSink<'a> = Box<dyn FnMut(AutoSwitchEvent) + 'a>;

#[derive(Debug, Clone, PartialEq)]
enum Freshen {
    Ok,
    InvalidGrant,
    /// Alive, but authenticates as a different account than the slot.
    IdentityConflict,
    Transient,
    SkipLiveSession,
    Systemic(&'static str),
}

/// Python `datetime.isoformat()` with `Z`: microseconds only when present.
fn iso_utc(ts: f64) -> String {
    let micros = (ts * 1e6).round() as i64;
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let base = format_timestamp(secs);
    if frac == 0 {
        base
    } else {
        format!("{}.{frac:06}Z", base.trim_end_matches('Z'))
    }
}

fn account_ref(number: &str, email: &str) -> Value {
    let number = number
        .parse::<i64>()
        .map_or_else(|_| Value::from(number), Value::from);
    json!({"number": number, "email": email})
}

fn usage_value(entry: &UsageEntry) -> UsageValue {
    match entry.decision_value() {
        DecisionValue::Usage(v) => UsageValue::Usage(v.clone()),
        DecisionValue::Sentinel(s) => UsageValue::Sentinel(s),
        DecisionValue::Unavailable => UsageValue::Unknown,
    }
}

fn entry_of<'e>(entries: &'e [(String, UsageEntry)], num: &str) -> Option<&'e UsageEntry> {
    entries.iter().find(|(n, _)| n == num).map(|(_, e)| e)
}

pub struct AutoSwitchEngine<'a> {
    host: &'a dyn EngineHost,
    settings: AutoSwitchSettings,
    on_event: EventSink<'a>,
    dry_run: bool,
    state: StateFile,
    clock: Clock,
    rng: Rng<'a>,
    models: Vec<String>,
    unhealthy_ticks: i64,
    sleep_until_ts: Option<f64>,
    blocked_wait_long: bool,
    idle_hold_since: Option<f64>,
    idle_hold_slow: bool,
    model_check_done: bool,
    handle: LoopHandle,
}

impl<'a> AutoSwitchEngine<'a> {
    /// Pins the host's poll planning to these settings' threshold and models.
    pub fn new(
        host: &'a dyn EngineHost,
        settings: AutoSwitchSettings,
        on_event: EventSink<'a>,
    ) -> Self {
        let models = settings.models();
        host.set_poll_policy_inputs(settings.threshold, &models);
        AutoSwitchEngine {
            state: StateFile::new(host.backup_root().join(STATE_FILENAME)),
            host,
            on_event,
            dry_run: false,
            clock: Arc::new(crate::usage::now_secs),
            rng: Box::new(default_rng()),
            unhealthy_ticks: 0,
            sleep_until_ts: None,
            blocked_wait_long: false,
            idle_hold_since: None,
            idle_hold_slow: false,
            model_check_done: models.is_empty(),
            handle: LoopHandle::new(),
            models,
            settings,
        }
    }

    /// Decide and report, but never switch, freshen or write state.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Wall time: persisted cooldown timestamps must survive processes.
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// `[0, 1)` source for the loop's jitter.
    pub fn rng(mut self, rng: Rng<'a>) -> Self {
        self.rng = rng;
        self
    }

    pub fn state_file(mut self, state: StateFile) -> Self {
        self.state = state;
        self
    }

    pub fn settings(&self) -> &AutoSwitchSettings {
        &self.settings
    }

    pub fn state(&self) -> &StateFile {
        &self.state
    }

    /// The known-reset wake time the last tick asked for.
    pub fn sleep_until_ts(&self) -> Option<f64> {
        self.sleep_until_ts
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    fn emit(&mut self, kind: EventKind) {
        let ts = format_timestamp(self.now().floor() as i64);
        (self.on_event)(AutoSwitchEvent { ts, kind });
    }

    fn no_switch(&mut self, reason: &'static str, detail: impl Into<String>) {
        self.emit(EventKind::NoSwitch {
            reason,
            detail: detail.into(),
        });
    }

    /// Never fails; an error is an `error` event and [`TickOutcome::Error`].
    pub fn tick(&mut self) -> TickOutcome {
        match self.tick_inner() {
            Ok(outcome) => outcome,
            Err(e) => {
                self.emit(EventKind::Error {
                    message: e.to_string(),
                    transient: true,
                });
                TickOutcome::Error
            }
        }
    }

    fn in_cooldown(&self, state: &State) -> bool {
        match state.get("lastSwitchAt") {
            Some(Value::Number(n)) => n
                .as_f64()
                .is_some_and(|last| self.now() - last < self.settings.cooldown_seconds),
            _ => false,
        }
    }

    fn quarantine(&mut self, number: &str, email: &str, reason: &str) -> Result<()> {
        let creds = self.host.read_account_credentials(number, email);
        let fingerprint = credential_fingerprint(&creds);
        let at = format_timestamp(self.now().floor() as i64);
        self.state.mutate(|s| {
            let entry = json!({
                "email": email,
                "reason": reason,
                "at": at,
                "refreshTokenFingerprint": fingerprint,
            });
            match s.get_mut("quarantine") {
                Some(Value::Object(q)) => {
                    q.insert(number.into(), entry);
                }
                _ => {
                    let mut q = serde_json::Map::new();
                    q.insert(number.into(), entry);
                    s.insert("quarantine".into(), Value::Object(q));
                }
            }
        })?;
        self.emit(EventKind::Quarantined {
            number: number.into(),
            email: email.into(),
            reason: reason.into(),
        });
        Ok(())
    }

    fn release_recovered_quarantines(&mut self, state: State) -> Result<State> {
        let Some(Value::Object(quarantine)) = state.get("quarantine") else {
            return Ok(state);
        };
        let mut released: Vec<(String, String, &'static str)> = Vec::new();
        for (number, entry) in quarantine {
            let recorded = entry
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let email_now = self.host.account_email(number);
            if email_now.is_empty() || email_now != recorded {
                released.push((number.clone(), recorded, "account-replaced"));
                continue;
            }
            let creds = self.host.read_account_credentials(number, &email_now);
            let fingerprint = credential_fingerprint(&creds).map(Value::from);
            if fingerprint.as_ref()
                != entry
                    .get("refreshTokenFingerprint")
                    .filter(|v| !v.is_null())
            {
                released.push((number.clone(), email_now, "credentials-replaced"));
            }
        }
        if released.is_empty() {
            return Ok(state);
        }
        let state = self.state.mutate(|s| {
            if let Some(Value::Object(q)) = s.get_mut("quarantine") {
                for (number, _, _) in &released {
                    q.shift_remove(number);
                }
            }
        })?;
        for (number, email, reason) in released {
            self.emit(EventKind::Unquarantined {
                number,
                email,
                reason: reason.into(),
            });
        }
        Ok(state)
    }

    /// Ensures a target's stored token outlives Claude Code's refresh buffer
    /// before activation. Only ever touches the slot's backup.
    fn freshen_target(&self, number: &str, email: &str) -> Freshen {
        if self.host.account_kind(number) == AccountKind::ApiKey {
            return Freshen::Ok;
        }
        if !self.host.live_session_pids_for(number, email).is_empty() {
            return Freshen::SkipLiveSession;
        }
        let creds = self.host.read_account_credentials(number, email);
        if creds.is_empty() {
            return Freshen::Transient;
        }
        let Some(data) = extract_oauth_data(&creds) else {
            return Freshen::InvalidGrant;
        };
        let near_expiry = match data.get("expiresAt") {
            Some(Value::Number(n)) => n
                .as_f64()
                .is_some_and(|exp| self.now() * 1000.0 + FRESHEN_BUFFER_MS >= exp),
            Some(Value::Bool(b)) => {
                self.now() * 1000.0 + FRESHEN_BUFFER_MS >= f64::from(u8::from(*b))
            }
            _ => false,
        };
        if !near_expiry {
            return Freshen::Ok;
        }
        let outcome = self.host.consume_backup_grant(number, email, &creds);
        match outcome.error.as_deref() {
            None if outcome
                .credentials
                .as_deref()
                .is_some_and(|c| !c.is_empty()) =>
            {
                if self.note_token_identity(number, outcome.token_account.as_ref()) {
                    Freshen::IdentityConflict
                } else {
                    Freshen::Ok
                }
            }
            Some("invalid_grant" | "no_refresh_token") => Freshen::InvalidGrant,
            Some(other) => match systemic_rank(other) {
                Some(i) => Freshen::Systemic(SYSTEMIC[i].0),
                None => Freshen::Transient,
            },
            None => Freshen::Transient,
        }
    }

    /// Checks the token endpoint's free identity against the slot: `true`
    /// on a conflict (org first, when both record one; then uuid). A blank
    /// slot uuid is backfilled, but never across an org conflict.
    fn note_token_identity(&self, number: &str, token_account: Option<&TokenAccount>) -> bool {
        let Some(account) = token_account.filter(|a| !a.uuid.trim().is_empty()) else {
            return false;
        };
        let slot = self.host.account_identity(number);
        if let Some(org) = account.organization_uuid.as_deref()
            && !org.is_empty()
            && !slot.organization_uuid.is_empty()
            && org != slot.organization_uuid
        {
            return true;
        }
        let uuid = account.uuid.trim();
        if slot.uuid.is_empty() {
            self.host.backfill_account_uuid(number, uuid);
            return false;
        }
        slot.uuid != uuid
    }

    fn decisions(&self, entries: &[(String, UsageEntry)]) -> Decisions {
        Decisions::new(
            entries
                .iter()
                .map(|(n, e)| (n.clone(), usage_value(e)))
                .collect(),
            &self.models,
        )
    }

    fn tick_inner(&mut self) -> Result<TickOutcome> {
        self.sleep_until_ts = None;
        self.blocked_wait_long = false;
        self.idle_hold_slow = false;
        let settings = self.settings.clone();
        let mut state = self.state.read();
        if !self.dry_run {
            state = self.release_recovered_quarantines(state)?;
        }
        let quarantined: HashSet<String> = match state.get("quarantine") {
            Some(Value::Object(q)) => q.keys().cloned().collect(),
            _ => HashSet::new(),
        };

        let Some(current) = self.host.current_account_number() else {
            self.emit(EventKind::Poll {
                active: None,
                headroom: Vec::new(),
                threshold: settings.threshold,
                fetch_errors: Vec::new(),
                windows: Vec::new(),
            });
            if self.host.has_live_login() {
                self.no_switch(
                    "unmanaged-active-account",
                    "run 'ccswap --add-account' to include it in rotation",
                );
            } else {
                self.no_switch(
                    "no-active-account",
                    "log in and run 'ccswap --add-account' first",
                );
            }
            return Ok(TickOutcome::NoAction);
        };

        let current_email = self.host.account_email(&current);
        let active_ref = account_ref(&current, &current_email);
        let (mut entries, mut decisions) =
            self.collect_scheduled_usage(&current, &quarantined, settings.threshold);
        self.emit_poll(active_ref, &entries, &decisions, settings.threshold);

        if !self.model_check_done {
            self.check_model_names(&quarantined, &decisions);
        }

        if self.host.account_kind(&current) == AccountKind::ApiKey
            && !settings.include_api_key_accounts
        {
            self.no_switch("active-api-key", "API-key accounts have no quota to watch");
            return Ok(TickOutcome::NoAction);
        }

        let mut active_headroom = decisions.headroom(&current);
        let trigger = match active_headroom {
            Some(h) => {
                self.unhealthy_ticks = 0;
                self.idle_hold_since = None;
                let utilization = 100.0 - h;
                if utilization < settings.threshold {
                    if settings.strategy != AutoStrategy::ConsumeFirst {
                        self.no_switch(
                            "below-threshold",
                            format!(
                                "{}% < {}%",
                                pct_label(utilization),
                                pct_label(settings.threshold)
                            ),
                        );
                        return Ok(TickOutcome::NoAction);
                    }
                    Trigger::ConsumeFirst
                } else if h <= 0.0 {
                    Trigger::AtLimit
                } else {
                    Trigger::Proactive
                }
            }
            None => {
                if decisions.value(&current) == Some(&UsageValue::Sentinel(Sentinel::TokenExpired))
                {
                    let now = self.now();
                    let since = *self.idle_hold_since.get_or_insert(now);
                    if now - since <= IDLE_HOLD_MAX_S {
                        self.unhealthy_ticks = 0;
                        self.idle_hold_slow = true;
                        self.no_switch(
                            "active-idle",
                            "token expired while Claude Code is idle; resumes on next use",
                        );
                        return Ok(TickOutcome::NoAction);
                    }
                    crate::log::warning(format!(
                        "Active token expired and owned for over {} minutes; resuming unhealthy counting (dead refresh token?)",
                        events::fixed0(IDLE_HOLD_MAX_S / 60.0)
                    ));
                } else {
                    self.idle_hold_since = None;
                }
                self.unhealthy_ticks += 1;
                if self.unhealthy_ticks < settings.unhealthy_ticks {
                    self.no_switch(
                        "active-usage-unknown",
                        format!(
                            "{}/{} before failover",
                            self.unhealthy_ticks, settings.unhealthy_ticks
                        ),
                    );
                    return Ok(TickOutcome::NoAction);
                }
                Trigger::Failover
            }
        };

        if trigger.is_optional() && self.in_cooldown(&state) {
            self.no_switch("cooldown", "");
            return Ok(TickOutcome::NoAction);
        }

        let candidates: Vec<String> = self
            .host
            .switchable_account_numbers()
            .into_iter()
            .filter(|n| *n != current && !quarantined.contains(n))
            .collect();
        let oauth_candidates: Vec<String> = candidates
            .iter()
            .filter(|n| self.host.account_kind(n) != AccountKind::ApiKey)
            .cloned()
            .collect();
        let api_key_candidates: Vec<String> = if settings.include_api_key_accounts {
            candidates
                .iter()
                .filter(|n| self.host.account_kind(n) == AccountKind::ApiKey)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if trigger == Trigger::ConsumeFirst
            && oauth_candidates.is_empty()
            && let Some(h) = active_headroom
        {
            self.no_switch(
                "below-threshold",
                format!(
                    "{}% < {}%",
                    pct_label(100.0 - h),
                    pct_label(settings.threshold)
                ),
            );
            return Ok(TickOutcome::NoAction);
        }
        if oauth_candidates.is_empty() && api_key_candidates.is_empty() {
            self.blocked_wait_long = true;
            self.no_switch("no-candidates", "");
            return Ok(TickOutcome::Blocked);
        }

        let consume_first = settings.strategy == AutoStrategy::ConsumeFirst;
        let mut decided_now = self.now();
        let mut ranked = self.rank(
            trigger,
            consume_first,
            &oauth_candidates,
            &state,
            &decisions,
            &current,
            active_headroom,
            decided_now,
        );

        if trigger == Trigger::ConsumeFirst && !ranked.ordered.is_empty() {
            let fetch: HashSet<String> = std::iter::once(current.clone())
                .chain(candidates.iter().cloned())
                .collect();
            entries = self.host.usage_entries(&fetch, false);
            decisions = self.decisions(&entries);
            active_headroom = decisions.headroom(&current);
            decided_now = self.now();
            ranked = self.rank(
                trigger,
                consume_first,
                &oauth_candidates,
                &state,
                &decisions,
                &current,
                active_headroom,
                decided_now,
            );
        }

        let Ranked {
            mut ordered,
            any_known,
            active_reset_ts,
        } = ranked;
        if ordered.is_empty() && !api_key_candidates.is_empty() && trigger != Trigger::ConsumeFirst
        {
            ordered = api_key_candidates;
        }

        if ordered.is_empty() {
            return Ok(self.nothing_qualifies(
                trigger,
                any_known,
                active_reset_ts,
                &oauth_candidates,
                &decisions,
            ));
        }

        let left = (
            active_headroom,
            binding_recovery_ts(decisions.dict(&current), &self.models, decided_now),
        );
        let mut transient_failure = false;
        let mut systemic: Option<&'static str> = None;
        for num in &ordered {
            let email = self.host.account_email(num);
            if trigger == Trigger::ConsumeFirst {
                let now = self.now();
                let fresh =
                    entry_of(&entries, num).is_some_and(|e| e.fresh(now, poll_policy::SERVE_TTL_S));
                if !fresh {
                    self.no_switch(
                        "stale-usage",
                        format!(
                            "account {num} usage could not be refreshed this tick (backoff or a concurrent poller); retrying"
                        ),
                    );
                    return Ok(TickOutcome::NoAction);
                }
            }
            if self.dry_run {
                return self.perform(num, &email, trigger, left);
            }
            match self.freshen_target(num, &email) {
                Freshen::IdentityConflict => self.quarantine(num, &email, "identity-conflict")?,
                Freshen::InvalidGrant => self.quarantine(num, &email, "invalid_grant")?,
                Freshen::Transient => transient_failure = true,
                Freshen::Systemic(status) => {
                    if systemic.is_none_or(|s| systemic_rank(status) < systemic_rank(s)) {
                        systemic = Some(status);
                    }
                }
                Freshen::SkipLiveSession => {}
                Freshen::Ok => return self.perform(num, &email, trigger, left),
            }
        }

        if let Some(status) = systemic {
            let message = SYSTEMIC
                .iter()
                .find(|(s, _)| *s == status)
                .map(|(_, m)| *m)
                .unwrap_or_default();
            self.emit(EventKind::Error {
                message: format!("could not freshen: {message}"),
                transient: true,
            });
            return Ok(TickOutcome::Error);
        }
        if transient_failure {
            self.emit(EventKind::Error {
                message: "could not freshen any candidate (network?)".into(),
                transient: true,
            });
            return Ok(TickOutcome::Error);
        }
        self.no_switch("no-viable-target", "");
        Ok(TickOutcome::Blocked)
    }

    fn emit_poll(
        &mut self,
        active: Value,
        entries: &[(String, UsageEntry)],
        decisions: &Decisions,
        threshold: f64,
    ) {
        let fetch_errors = entries
            .iter()
            .filter(|(n, _)| decisions.value(n).is_none_or(|v| *v == UsageValue::Unknown))
            .filter_map(|(n, e)| {
                e.last_error
                    .clone()
                    .filter(|err| !err.is_empty())
                    .map(|err| (n.clone(), err))
            })
            .collect();
        let windows = decisions
            .usage
            .iter()
            .filter_map(|(n, v)| {
                let UsageValue::Usage(u) = v else {
                    return None;
                };
                let mut pcts: Vec<(String, f64)> = Vec::new();
                for w in relevant_windows(Some(u), &self.models) {
                    match pcts.iter_mut().find(|(l, _)| *l == w.label) {
                        Some(slot) => slot.1 = w.pct,
                        None => pcts.push((w.label, w.pct)),
                    }
                }
                (!pcts.is_empty()).then(|| (n.clone(), pcts))
            })
            .collect();
        self.emit(EventKind::Poll {
            active: Some(active),
            headroom: decisions.headroom.clone(),
            threshold,
            fetch_errors,
            windows,
        });
    }

    /// Ranks with the no-return bar, and without it when that empties the
    /// list and the account we left has recovered since.
    #[allow(clippy::too_many_arguments)]
    fn rank(
        &self,
        trigger: Trigger,
        consume_first: bool,
        oauth_candidates: &[String],
        state: &State,
        decisions: &Decisions,
        current: &str,
        active_headroom: Option<f64>,
        now: f64,
    ) -> Ranked {
        let bar = BarInput {
            state,
            decisions,
            active_headroom,
            threshold: self.settings.threshold,
            models: &self.models,
            now,
            current,
        };
        let recovered = rank::left_account_recovered(&bar);
        let no_return = rank::no_return_account(trigger, recovered, &bar);
        let input = |no_return| RankInput {
            trigger,
            consume_first,
            oauth_candidates,
            no_return,
            decisions,
            current,
            active_headroom,
            threshold: self.settings.threshold,
            hysteresis_pct: self.settings.hysteresis_pct,
            models: &self.models,
            now,
        };
        let ranked = rank::rank_candidates(&input(no_return.as_deref()));
        if no_return.is_some() && ranked.ordered.is_empty() && recovered {
            let unbarred = rank::rank_candidates(&input(None));
            if !unbarred.ordered.is_empty() {
                return unbarred;
            }
        }
        ranked
    }

    fn nothing_qualifies(
        &mut self,
        trigger: Trigger,
        any_known: bool,
        active_reset_ts: Option<f64>,
        oauth_candidates: &[String],
        decisions: &Decisions,
    ) -> TickOutcome {
        if !any_known {
            self.no_switch("no-comparison", "no candidate has readable usage");
            return TickOutcome::Blocked;
        }
        if trigger == Trigger::ConsumeFirst {
            if active_reset_ts.is_none() {
                self.no_switch(
                    "reset-unknown",
                    "active account's weekly reset time is unknown; consume-first is idle until it is reported",
                );
            } else {
                self.no_switch(
                    "already-consuming-soonest",
                    "no sooner-resetting account with room to spare",
                );
            }
            return TickOutcome::NoAction;
        }
        let truly_exhausted = oauth_candidates
            .iter()
            .all(|n| decisions.headroom(n).is_some_and(|h| h <= 0.0));
        if !truly_exhausted {
            self.no_switch(
                "no-qualifying-candidate",
                "no candidate is below the threshold and better than the active account by the hysteresis margin, or usage is unreadable this tick",
            );
            return TickOutcome::Blocked;
        }
        self.blocked_wait_long = true;
        let earliest = self.earliest_recovery(decisions);
        if let Some(ts) = earliest {
            self.sleep_until_ts = Some(ts + RESET_SLACK_S);
        }
        self.emit(EventKind::AllExhausted {
            earliest_reset_at: earliest.map(iso_utc),
        });
        TickOutcome::Blocked
    }

    /// Phase A fetches the active slot when due plus one due candidate;
    /// phase B refetches everyone when a switch could be near.
    fn collect_scheduled_usage(
        &mut self,
        current: &str,
        quarantined: &HashSet<String>,
        threshold: f64,
    ) -> (Vec<(String, UsageEntry)>, Decisions) {
        let now = self.now();
        let candidates: Vec<String> = self
            .host
            .switchable_account_numbers()
            .into_iter()
            .filter(|n| n != current && !quarantined.contains(n))
            .collect();

        let pre = self.host.usage_entries(&HashSet::new(), false);
        let mut plan: HashSet<String> = HashSet::new();
        let active_pre = entry_of(&pre, current);
        let stale_candidate_plan = active_pre.is_some_and(|e| {
            e.age_s
                .is_some_and(|age| age >= poll_policy::ACTIVE_MAX_INTERVAL_S)
                && e.poll_interval_s.unwrap_or(0.0) > poll_policy::ACTIVE_MAX_INTERVAL_S
                && binding_pct(e.last_good.as_ref(), &self.models).unwrap_or(0.0) < 100.0
        });
        let overslept = active_pre.is_some_and(|e| plan_oversleeps_interval(e, now));
        let nominate = match active_pre {
            None => true,
            Some(e) => match e.age_s {
                None => true,
                Some(age) => {
                    stale_candidate_plan
                        || overslept
                        || e.next_poll_at.is_some_and(|next| now >= next)
                        || (e.next_poll_at.is_none() && age >= poll_policy::MIN_INTERVAL_S)
                }
            },
        };
        if nominate {
            plan.insert(current.to_string());
        }
        if self.idle_hold_since.is_none() {
            let pre_map: HashMap<String, UsageEntry> = pre.iter().cloned().collect();
            if let Some(pick) = due_candidate(&candidates, &pre_map, now) {
                plan.insert(pick);
            }
        }
        let mut entries = self.host.usage_entries(&plan, !stale_candidate_plan);
        let mut decisions = self.decisions(&entries);

        let active_value = decisions.value(current);
        let active_headroom = decisions.headroom(current);
        let escalate = !candidates.is_empty()
            && match active_headroom {
                None => active_value != Some(&UsageValue::Sentinel(Sentinel::TokenExpired)),
                Some(h) => 100.0 - h >= threshold - ESCALATION_MARGIN_PCT,
            };
        if escalate {
            let mut fetch: HashSet<String> = std::iter::once(current.to_string())
                .chain(candidates.iter().cloned())
                .collect();
            fetch.retain(|num| {
                let Some(entry) = entry_of(&entries, num) else {
                    return true;
                };
                let parked = entry.next_poll_at.is_some_and(|next| now < next)
                    && entry.poll_interval_s.unwrap_or(0.0) > poll_policy::EXHAUSTED_INTERVAL_S
                    && decisions.headroom(num).is_some_and(|h| h <= 0.0);
                !parked
            });
            entries = self.host.usage_entries(&fetch, false);
            decisions = self.decisions(&entries);
        }
        (entries, decisions)
    }

    fn perform(
        &mut self,
        number: &str,
        email: &str,
        trigger: Trigger,
        left: (Option<f64>, f64),
    ) -> Result<TickOutcome> {
        if self.dry_run {
            let from = self
                .host
                .current_account_number()
                .map(|c| account_ref(&c, &self.host.account_email(&c)));
            self.emit(EventKind::Switch {
                trigger,
                from,
                to: Some(account_ref(number, email)),
                warnings: Vec::new(),
                dry_run: true,
            });
            return Ok(TickOutcome::Switched);
        }

        let result = {
            let _lock = self.state.lock()?;
            let mut state = self.state.read();
            if trigger.is_optional() && self.in_cooldown(&state) {
                drop(_lock);
                self.no_switch("cooldown", "");
                return Ok(TickOutcome::NoAction);
            }
            let result = self.host.switch_to(number)?;
            if !result.switched {
                drop(_lock);
                self.no_switch("already-active", result.reason.clone());
                return Ok(TickOutcome::NoAction);
            }
            state.insert("schemaVersion".into(), STATE_SCHEMA_VERSION.into());
            state.insert("lastSwitchAt".into(), json!(self.now()));
            state.insert("lastSwitchTo".into(), number.into());
            state.insert(
                "lastSwitchFrom".into(),
                result
                    .from
                    .as_ref()
                    .and_then(|f| f.get("number"))
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            state.insert("leftHeadroom".into(), json!(left.0));
            state.insert(
                "leftRecoveryAt".into(),
                if left.1.is_infinite() {
                    Value::Null
                } else {
                    json!(left.1)
                },
            );
            state.insert("leftTrigger".into(), trigger.as_str().into());
            self.state.write(&state)?;
            result
        };
        self.emit(EventKind::Switch {
            trigger,
            from: result.from,
            to: result.to,
            warnings: result.warnings,
            dry_run: false,
        });
        Ok(TickOutcome::Switched)
    }

    /// One-shot `autoswitch.model` typo guard, once every relevant account
    /// has readable usage.
    fn check_model_names(&mut self, quarantined: &HashSet<String>, decisions: &Decisions) {
        let wanted: Vec<&String> = self
            .models
            .iter()
            .filter(|m| m.to_lowercase() != "all")
            .collect();
        if wanted.is_empty() {
            self.model_check_done = true;
            return;
        }
        let relevant: Vec<String> = self
            .host
            .switchable_account_numbers()
            .into_iter()
            .filter(|n| {
                !quarantined.contains(n) && self.host.account_kind(n) != AccountKind::ApiKey
            })
            .collect();
        let readable: Vec<&Value> = relevant.iter().filter_map(|n| decisions.dict(n)).collect();
        if readable.is_empty() || readable.len() != relevant.len() {
            return;
        }
        let seen: HashSet<String> = readable
            .iter()
            .filter_map(|v| v.get("scoped").and_then(Value::as_array))
            .flatten()
            .filter_map(|s| s.get("name").and_then(Value::as_str))
            .map(str::to_lowercase)
            .collect();
        self.model_check_done = true;
        let missing: Vec<String> = wanted
            .into_iter()
            .filter(|m| !seen.contains(&m.to_lowercase()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            self.emit(EventKind::ConfigWarning {
                message: format!(
                    "autoswitch.model: {} matches no account's usage windows — only the 5h/7d limits are being watched for it (typo?)",
                    missing.join(", ")
                ),
            });
        }
    }

    /// Earliest moment any exhausted account is usable again, or `None` when
    /// that cannot be proven.
    fn earliest_recovery(&self, decisions: &Decisions) -> Option<f64> {
        let now = self.now();
        let mut earliest: Option<f64> = None;
        for (_, value) in &decisions.usage {
            let UsageValue::Usage(u) = value else {
                continue;
            };
            if !relevant_windows(Some(u), &self.models)
                .iter()
                .any(|w| w.pct >= 100.0)
            {
                continue;
            }
            let usable_at = limiting_reset_ts(Some(u), &self.models).filter(|t| *t > now)?;
            earliest = Some(earliest.map_or(usable_at, |e| e.min(usable_at)));
        }
        earliest
    }

    pub fn next_delay(&mut self, outcome: TickOutcome) -> f64 {
        let interval = self.settings.interval_seconds;
        match outcome {
            TickOutcome::Blocked => {
                if let Some(until) = self.sleep_until_ts {
                    return (until - self.now()).max(interval).min(MAX_SLEEP_S);
                }
                if self.blocked_wait_long {
                    return interval.max(NO_RESET_FALLBACK_S);
                }
            }
            TickOutcome::NoAction if self.idle_hold_slow => {
                return interval.max(NO_RESET_FALLBACK_S);
            }
            _ => {}
        }
        let jittered = interval * (0.9 + 0.2 * (self.rng)());
        self.respect_poll_plan(jittered)
    }

    /// Shortens a normal-cadence sleep to the active row's planned poll,
    /// never below the planner's urgent floor.
    fn respect_poll_plan(&self, delay: f64) -> f64 {
        let Some(current) = self.host.current_account_number() else {
            return delay;
        };
        let entries = self.host.usage_entries(&HashSet::new(), false);
        let Some(next) = entry_of(&entries, &current).and_then(|e| e.next_poll_at) else {
            return delay;
        };
        delay.min((next - self.now()).max(poll_policy::URGENT_INTERVAL_S))
    }
}

fn default_rng() -> impl FnMut() -> f64 {
    let mut state = crate::usage::now_secs().to_bits() ^ (u64::from(std::process::id()) << 32);
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    }
}
