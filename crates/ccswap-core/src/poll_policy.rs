//! When to poll the usage endpoint next (upstream `poll_policy.py`). The
//! endpoint allows roughly 30 requests per trailing hour per identity, so
//! every surface shares one adaptive plan per account.

use serde_json::Value;

use crate::usage::{account_headroom, parse_reset_ts, relevant_windows};

pub const SERVE_TTL_S: f64 = 180.0;
pub const MIN_INTERVAL_S: f64 = 180.0;
pub const URGENT_INTERVAL_S: f64 = 60.0;
pub const ACTIVE_MAX_INTERVAL_S: f64 = 300.0;
pub const CANDIDATE_DEFAULT_INTERVAL_S: f64 = 300.0;
pub const CANDIDATE_MAX_INTERVAL_S: f64 = 600.0;
pub const EXHAUSTED_INTERVAL_S: f64 = 600.0;
pub const MOVEMENT_DELTA_PCT: f64 = 1.0;
pub const JITTER_FRAC: f64 = 0.1;
pub const EDGE_BACKOFF_S: f64 = 300.0;
pub const POST_429_MIN_INTERVAL_S: f64 = 360.0;
pub const RECENT_429_WINDOW_S: f64 = 3600.0;
pub const POST_429_BACKOFF_MULT: f64 = 1.5;
pub const POST_429_MAX_INTERVAL_S: f64 = 1800.0;
pub const ESCALATION_MARGIN_PCT: f64 = 15.0;
pub const RESET_SLACK_S: f64 = 60.0;

pub fn binding_pct(usage: Option<&Value>, models: &[String]) -> Option<f64> {
    account_headroom(usage, models).map(|h| 100.0 - h)
}

/// When the last of the at-limit windows resets.
pub fn limiting_reset_ts(usage: Option<&Value>, models: &[String]) -> Option<f64> {
    relevant_windows(usage, models)
        .iter()
        .filter(|w| w.pct >= 100.0)
        .filter_map(|w| parse_reset_ts(w.resets_at.as_ref()))
        .reduce(f64::max)
}

/// The next window reset after `now`, at any utilization.
pub fn earliest_future_reset_ts(usage: Option<&Value>, now: f64, models: &[String]) -> Option<f64> {
    relevant_windows(usage, models)
        .iter()
        .filter_map(|w| parse_reset_ts(w.resets_at.as_ref()))
        .filter(|ts| *ts > now)
        .reduce(f64::min)
}

pub struct PlanInputs<'a> {
    pub prev_interval_s: Option<f64>,
    pub prev_usage: Option<&'a Value>,
    pub new_usage: Option<&'a Value>,
    pub is_active: bool,
    pub threshold: f64,
    pub models: &'a [String],
    pub recent_429: bool,
    pub now: f64,
}

/// `(next_poll_at, interval_s)` after a successful fetch; `rng` yields
/// `[0, 1)` and only jitters `next_poll_at`.
pub fn plan_after_fetch(p: &PlanInputs, rng: &mut dyn FnMut() -> f64) -> (f64, f64) {
    let default = if p.is_active {
        MIN_INTERVAL_S
    } else {
        CANDIDATE_DEFAULT_INTERVAL_S
    };
    let ceiling = if p.is_active {
        ACTIVE_MAX_INTERVAL_S
    } else {
        CANDIDATE_MAX_INTERVAL_S
    };
    let base = p.prev_interval_s.filter(|i| *i != 0.0).unwrap_or(default);
    let prev_pct = binding_pct(p.prev_usage, p.models);
    let new_pct = binding_pct(p.new_usage, p.models);
    let (mut interval, moving) = match (prev_pct, new_pct) {
        (Some(prev), Some(new)) if (new - prev).abs() >= MOVEMENT_DELTA_PCT => {
            (MIN_INTERVAL_S.max(base / 2.0), true)
        }
        (Some(_), Some(_)) => (ceiling.min(MIN_INTERVAL_S.max(base * 1.5)), false),
        _ => (default, false),
    };
    if p.is_active
        && moving
        && !p.recent_429
        && new_pct.is_some_and(|n| n >= p.threshold - ESCALATION_MARGIN_PCT)
    {
        interval = URGENT_INTERVAL_S;
    }
    if p.recent_429 {
        let increased = (base * POST_429_BACKOFF_MULT).max(POST_429_MIN_INTERVAL_S);
        interval = POST_429_MAX_INTERVAL_S.min(interval.max(increased));
    }
    let headroom = account_headroom(p.new_usage, p.models);
    let exhausted = headroom.is_some_and(|h| h <= 0.0);
    if exhausted {
        interval = interval.max(EXHAUSTED_INTERVAL_S);
    }
    let mut next = p.now + interval * (1.0 + JITTER_FRAC * (2.0 * rng() - 1.0));
    if exhausted {
        if let Some(reset) = limiting_reset_ts(p.new_usage, p.models).filter(|r| *r > p.now) {
            next = next.min(reset + RESET_SLACK_S);
        }
    } else if let Some(reset) = earliest_future_reset_ts(p.new_usage, p.now, p.models) {
        next = next.min(reset + RESET_SLACK_S);
    }
    (next, interval)
}

#[cfg(test)]
mod tests;
