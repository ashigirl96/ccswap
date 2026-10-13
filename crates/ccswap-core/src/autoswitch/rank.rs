//! Pure candidate ranking and the no-return bar: no events, no writes, so
//! the consume-first two-phase commit can rank twice per tick.

use serde_json::Value;

use super::events::Trigger;
use super::state::State;
use crate::usage::{Sentinel, account_headroom, parse_reset_ts, relevant_windows};

/// Anti-flap margin on the recovery axis.
pub const RECOVERY_HYSTERESIS_S: f64 = 300.0;
/// Past this a sooner reset stops being worth real headroom.
pub const RECOVERY_HORIZON_S: f64 = 4.0 * 3600.0;
/// Anti-flap margin on the headroom axis, as a ratio.
pub const HORIZON_HEADROOM_RATIO: f64 = 2.0;
/// Below this an account is spent; headroom edges between spent accounts
/// are noise.
pub const SPENT_HEADROOM_PCT: f64 = 3.0;

/// A slot's decision value: a measurement, a sentinel, or unknown.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageValue {
    Usage(Value),
    Sentinel(Sentinel),
    Unknown,
}

/// Decision values and the headroom derived from them, in sequence order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decisions {
    pub usage: Vec<(String, UsageValue)>,
    pub headroom: Vec<(String, Option<f64>)>,
}

impl Decisions {
    pub fn new(usage: Vec<(String, UsageValue)>, models: &[String]) -> Self {
        let headroom = usage
            .iter()
            .map(|(n, v)| {
                let dict = match v {
                    UsageValue::Usage(u) => Some(u),
                    _ => None,
                };
                (n.clone(), account_headroom(dict, models))
            })
            .collect();
        Decisions { usage, headroom }
    }

    pub fn value(&self, num: &str) -> Option<&UsageValue> {
        self.usage.iter().find(|(n, _)| n == num).map(|(_, v)| v)
    }

    /// The measurement, when there is one.
    pub fn dict(&self, num: &str) -> Option<&Value> {
        match self.value(num) {
            Some(UsageValue::Usage(u)) => Some(u),
            _ => None,
        }
    }

    pub fn headroom(&self, num: &str) -> Option<f64> {
        self.headroom
            .iter()
            .find(|(n, _)| n == num)
            .and_then(|(_, h)| *h)
    }
}

/// The weekly window's reset, or `None` when unknown or already past.
pub fn seven_day_reset_ts(usage: Option<&Value>, now: f64) -> Option<f64> {
    let window = usage?.get("seven_day")?;
    if !window.is_object() {
        return None;
    }
    parse_reset_ts(window.get("resets_at")).filter(|ts| *ts > now)
}

/// When the binding (highest-pct) window comes back; `inf` when unknown or
/// past so such accounts sort last.
pub fn binding_recovery_ts(usage: Option<&Value>, models: &[String], now: f64) -> f64 {
    let windows = relevant_windows(usage, models);
    let Some(binding) = windows
        .iter()
        .reduce(|a, b| if b.pct > a.pct { b } else { a })
    else {
        return f64::INFINITY;
    };
    parse_reset_ts(binding.resets_at.as_ref())
        .filter(|ts| *ts > now)
        .unwrap_or(f64::INFINITY)
}

/// The active account and every measured candidate at or over the
/// threshold. Needs the active headroom and at least one measured candidate.
pub fn every_account_above_threshold(
    candidates: &[String],
    decisions: &Decisions,
    active_headroom: Option<f64>,
    threshold: f64,
) -> bool {
    let Some(active) = active_headroom else {
        return false;
    };
    if 100.0 - active < threshold {
        return false;
    }
    let measured: Vec<f64> = candidates
        .iter()
        .filter_map(|n| decisions.headroom(n))
        .collect();
    !measured.is_empty() && measured.iter().all(|h| 100.0 - h >= threshold)
}

/// Rank this candidate by soonest reset rather than by headroom? Decided per
/// pair, symmetric under a swap, so the axis survives the move.
pub fn recovery_is_useful(
    candidate_recovery_ts: f64,
    active_recovery_ts: f64,
    active_headroom: f64,
    best_candidate_headroom: f64,
    now: f64,
) -> bool {
    if active_headroom <= SPENT_HEADROOM_PCT && best_candidate_headroom <= SPENT_HEADROOM_PCT {
        return true;
    }
    candidate_recovery_ts - now <= RECOVERY_HORIZON_S
        || active_recovery_ts - now <= RECOVERY_HORIZON_S
}

pub struct RankInput<'a> {
    pub trigger: Trigger,
    pub consume_first: bool,
    pub oauth_candidates: &'a [String],
    pub no_return: Option<&'a str>,
    pub decisions: &'a Decisions,
    pub current: &'a str,
    pub active_headroom: Option<f64>,
    pub threshold: f64,
    pub hysteresis_pct: f64,
    pub models: &'a [String],
    pub now: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub ordered: Vec<String>,
    /// Some candidate had readable usage.
    pub any_known: bool,
    /// The active account's weekly reset (consume-first only).
    pub active_reset_ts: Option<f64>,
}

fn cmp_keys(a: &[f64], b: &[f64]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        match x.total_cmp(y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// Filters and orders the OAuth candidates for this trigger; ties keep
/// sequence order.
pub fn rank_candidates(input: &RankInput) -> Ranked {
    let d = input.decisions;
    let now = input.now;
    let active_reset_ts = if input.consume_first {
        seven_day_reset_ts(d.dict(input.current), now)
    } else {
        None
    };
    let all_above = every_account_above_threshold(
        input.oauth_candidates,
        d,
        input.active_headroom,
        input.threshold,
    );
    let best_candidate_headroom = input
        .oauth_candidates
        .iter()
        .filter_map(|n| d.headroom(n))
        .reduce(f64::max)
        .unwrap_or(0.0);
    let active_recovery_ts = if all_above {
        binding_recovery_ts(d.dict(input.current), input.models, now)
    } else {
        0.0
    };
    let active_h = input.active_headroom.unwrap_or(0.0);

    let mut qualifying: Vec<(Vec<f64>, String)> = Vec::new();
    let mut fallback: Vec<(Vec<f64>, String)> = Vec::new();
    let mut any_known = false;
    for num in input.oauth_candidates {
        let Some(h) = d.headroom(num) else {
            continue;
        };
        any_known = true;
        if h <= 0.0 || input.no_return == Some(num.as_str()) {
            continue;
        }
        let reset_ts = if input.consume_first {
            seven_day_reset_ts(d.dict(num), now)
        } else {
            None
        };
        let recovery_ts = if all_above {
            binding_recovery_ts(d.dict(num), input.models, now)
        } else {
            0.0
        };
        let mut by_recovery = false;
        if input.trigger.is_optional() {
            if 100.0 - h >= input.threshold && !all_above {
                continue;
            }
            if all_above {
                by_recovery = recovery_is_useful(
                    recovery_ts,
                    active_recovery_ts,
                    active_h,
                    best_candidate_headroom,
                    now,
                );
                if by_recovery {
                    if recovery_ts >= active_recovery_ts - RECOVERY_HYSTERESIS_S {
                        continue;
                    }
                } else if h < active_h * HORIZON_HEADROOM_RATIO {
                    if active_h <= SPENT_HEADROOM_PCT
                        && h >= active_h
                        && recovery_ts < active_recovery_ts - RECOVERY_HYSTERESIS_S
                    {
                        fallback.push((vec![0.0, recovery_ts, -h], num.clone()));
                    }
                    continue;
                }
            } else if input.consume_first {
                if input.trigger == Trigger::ConsumeFirst
                    && !matches!((reset_ts, active_reset_ts), (Some(r), Some(a)) if r < a)
                {
                    continue;
                }
            } else if let Some(active) = input.active_headroom
                && h - active < input.hysteresis_pct
            {
                continue;
            }
        }
        let key = if all_above && input.trigger.is_optional() {
            if by_recovery {
                vec![0.0, recovery_ts, -h]
            } else {
                vec![1.0, -h, recovery_ts]
            }
        } else if input.consume_first {
            vec![reset_ts.unwrap_or(f64::INFINITY), -h]
        } else {
            vec![-h]
        };
        qualifying.push((key, num.clone()));
    }
    let mut chosen = if qualifying.is_empty() {
        fallback
    } else {
        qualifying
    };
    chosen.sort_by(|a, b| cmp_keys(&a.0, &b.0));
    Ranked {
        ordered: chosen.into_iter().map(|(_, n)| n).collect(),
        any_known,
        active_reset_ts,
    }
}

/// Python `str()` of a stored slot number.
pub(crate) fn slot_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => crate::pyjson::float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

fn present(state: &State, key: &str) -> Option<Value> {
    state.get(key).filter(|v| !v.is_null()).cloned()
}

fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Bool(b)) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

pub struct BarInput<'a> {
    pub state: &'a State,
    pub decisions: &'a Decisions,
    pub active_headroom: Option<f64>,
    pub threshold: f64,
    pub models: &'a [String],
    pub now: f64,
    pub current: &'a str,
}

/// Is the account we last left a better proposition than when we left it?
pub fn left_account_recovered(b: &BarInput) -> bool {
    let Some(came_from) = present(b.state, "lastSwitchFrom") else {
        return true;
    };
    let barred = slot_str(&came_from);
    if !b.state.contains_key("leftHeadroom") {
        return true;
    }
    let h = b.decisions.headroom(&barred);
    let left_headroom = b.state.get("leftHeadroom").filter(|v| !v.is_null());
    let left_recovery = b.state.get("leftRecoveryAt").filter(|v| !v.is_null());
    let is_failover_snapshot = match b.state.get("leftTrigger").filter(|v| !v.is_null()) {
        Some(trigger) => trigger.as_str() == Some("failover"),
        None => left_headroom.is_none() && left_recovery.is_none(),
    };
    if is_failover_snapshot {
        if h.is_some_and(|h| h > 100.0 - b.threshold) {
            return true;
        }
        let peer = binding_recovery_ts(b.decisions.dict(&barred), b.models, b.now);
        let active = binding_recovery_ts(b.decisions.dict(b.current), b.models, b.now);
        return (active.is_finite() || peer - b.now <= RECOVERY_HORIZON_S)
            && peer < active - RECOVERY_HYSTERESIS_S;
    }
    if let Some(h) = h {
        match b.active_headroom {
            Some(active) => {
                if h > active * HORIZON_HEADROOM_RATIO + SPENT_HEADROOM_PCT {
                    return true;
                }
            }
            None => {
                if h > 100.0 - b.threshold {
                    return true;
                }
            }
        }
        if let Some(left) = number(left_headroom)
            && h >= (left + SPENT_HEADROOM_PCT).min(100.0)
        {
            return true;
        }
    }
    let was = number(left_recovery).unwrap_or(f64::INFINITY);
    binding_recovery_ts(b.decisions.dict(&barred), b.models, b.now) < was - RECOVERY_HYSTERESIS_S
}

/// The account this engine most recently left, while it is still barred.
pub fn no_return_account(trigger: Trigger, recovered: bool, b: &BarInput) -> Option<String> {
    let came_from = present(b.state, "lastSwitchFrom")?;
    if !trigger.is_optional() {
        return None;
    }
    if let Some(landed_on) = present(b.state, "lastSwitchTo")
        && slot_str(&landed_on) != b.current
    {
        return None;
    }
    let barred = slot_str(&came_from);
    if !recovered {
        return Some(barred);
    }
    if let Some(left) = b.decisions.headroom(&barred) {
        match b.active_headroom {
            Some(active) => {
                if left >= active * HORIZON_HEADROOM_RATIO {
                    return None;
                }
            }
            None => {
                if left > 100.0 - b.threshold {
                    return None;
                }
            }
        }
    }
    Some(barred)
}
