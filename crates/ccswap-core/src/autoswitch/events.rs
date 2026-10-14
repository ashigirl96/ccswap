//! Every engine outcome is one event, rendered as a human line or a JSONL object.

use crate::pyjson::py_str;
use serde_json::{Map, Value, json};

use crate::presentation::SCHEMA_VERSION;

/// Why the engine wanted to move off the active account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Proactive,
    AtLimit,
    Failover,
    ConsumeFirst,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Proactive => "proactive",
            Trigger::AtLimit => "at-limit",
            Trigger::Failover => "failover",
            Trigger::ConsumeFirst => "consume-first",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "proactive" => Trigger::Proactive,
            "at-limit" => Trigger::AtLimit,
            "failover" => Trigger::Failover,
            "consume-first" => Trigger::ConsumeFirst,
            _ => return None,
        })
    }

    /// The triggers the anti-flap gates apply to; at-limit and failover are
    /// escapes.
    pub fn is_optional(self) -> bool {
        matches!(self, Trigger::Proactive | Trigger::ConsumeFirst)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    Poll {
        /// `{"number": int, "email": str}`, or `None` without an active slot.
        active: Option<Value>,
        /// Slot → headroom pct, `None` when unknown; sequence order.
        headroom: Vec<(String, Option<f64>)>,
        threshold: f64,
        /// Slot → last fetch error, for slots whose usage is unknown.
        fetch_errors: Vec<(String, String)>,
        /// Slot → window label → pct, restricted to the decision's windows.
        windows: Vec<(String, Vec<(String, f64)>)>,
    },
    Switch {
        trigger: Trigger,
        from: Option<Value>,
        to: Option<Value>,
        warnings: Vec<String>,
        dry_run: bool,
    },
    NoSwitch {
        reason: &'static str,
        detail: String,
    },
    Quarantined {
        number: String,
        email: String,
        reason: String,
    },
    Unquarantined {
        number: String,
        email: String,
        reason: String,
    },
    AllExhausted {
        earliest_reset_at: Option<String>,
    },
    Sleep {
        seconds: f64,
        until: String,
    },
    Error {
        message: String,
        transient: bool,
    },
    ConfigWarning {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoSwitchEvent {
    /// UTC `YYYY-MM-DDTHH:MM:SSZ`.
    pub ts: String,
    pub kind: EventKind,
}

impl AutoSwitchEvent {
    pub fn name(&self) -> &'static str {
        match self.kind {
            EventKind::Poll { .. } => "poll",
            EventKind::Switch { .. } => "switch",
            EventKind::NoSwitch { .. } => "no-switch",
            EventKind::Quarantined { .. } => "account-quarantined",
            EventKind::Unquarantined { .. } => "account-unquarantined",
            EventKind::AllExhausted { .. } => "all-exhausted",
            EventKind::Sleep { .. } => "sleep",
            EventKind::Error { .. } => "error",
            EventKind::ConfigWarning { .. } => "config-warning",
        }
    }

    /// The JSONL payload; consumers ignore unknown kinds and fields.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("schemaVersion".into(), SCHEMA_VERSION.into());
        out.insert("event".into(), self.name().into());
        out.insert("ts".into(), self.ts.clone().into());
        let mut put = |k: &str, v: Value| {
            out.insert(k.into(), v);
        };
        match &self.kind {
            EventKind::Poll {
                active,
                headroom,
                threshold,
                fetch_errors,
                windows,
            } => {
                put("active", active.clone().unwrap_or(Value::Null));
                put(
                    "headroomPct",
                    Value::Object(
                        headroom
                            .iter()
                            .map(|(n, h)| (n.clone(), json!(h)))
                            .collect(),
                    ),
                );
                put("threshold", json!(threshold));
                if !fetch_errors.is_empty() {
                    put(
                        "fetchErrors",
                        Value::Object(
                            fetch_errors
                                .iter()
                                .map(|(n, e)| (n.clone(), e.clone().into()))
                                .collect(),
                        ),
                    );
                }
                if !windows.is_empty() {
                    put(
                        "windowsPct",
                        Value::Object(
                            windows
                                .iter()
                                .map(|(n, w)| {
                                    let pcts: Map<String, Value> =
                                        w.iter().map(|(l, p)| (l.clone(), json!(p))).collect();
                                    (n.clone(), Value::Object(pcts))
                                })
                                .collect(),
                        ),
                    );
                }
            }
            EventKind::Switch {
                trigger,
                from,
                to,
                warnings,
                dry_run,
            } => {
                put("trigger", trigger.as_str().into());
                put("from", from.clone().unwrap_or(Value::Null));
                put("to", to.clone().unwrap_or(Value::Null));
                put("warnings", json!(warnings));
                put("dryRun", (*dry_run).into());
            }
            EventKind::NoSwitch { reason, detail } => {
                put("reason", (*reason).into());
                put("detail", detail.clone().into());
            }
            EventKind::Quarantined {
                number,
                email,
                reason,
            }
            | EventKind::Unquarantined {
                number,
                email,
                reason,
            } => {
                put("number", number.clone().into());
                put("email", email.clone().into());
                put("reason", reason.clone().into());
            }
            EventKind::AllExhausted { earliest_reset_at } => {
                put("earliestResetAt", json!(earliest_reset_at));
            }
            EventKind::Sleep { seconds, until } => {
                put("seconds", json!(py_round1(*seconds)));
                put("until", until.clone().into());
            }
            EventKind::Error { message, transient } => {
                put("message", message.clone().into());
                put("transient", (*transient).into());
            }
            EventKind::ConfigWarning { message } => {
                put("message", message.clone().into());
            }
        }
        Value::Object(out)
    }

    /// The human line, without its timestamp prefix.
    pub fn human(&self) -> String {
        match &self.kind {
            EventKind::Poll {
                active,
                headroom,
                threshold,
                fetch_errors,
                windows,
            } => poll_human(active.as_ref(), headroom, *threshold, fetch_errors, windows),
            EventKind::Switch {
                trigger,
                from,
                to,
                dry_run,
                ..
            } => {
                let src = from.as_ref().map_or("(none)".into(), |f| {
                    format!("Account-{}", py_str(&f["number"]))
                });
                let dst = to.as_ref().map_or("?".into(), |t| {
                    format!("Account-{} ({})", py_str(&t["number"]), py_str(&t["email"]))
                });
                let prefix = if *dry_run {
                    "[dry-run] would switch"
                } else {
                    "Switched"
                };
                format!("{prefix} {src} -> {dst} ({})", trigger.as_str())
            }
            EventKind::NoSwitch { reason, detail } => {
                if detail.is_empty() {
                    format!("no switch: {reason}")
                } else {
                    format!("no switch: {reason} ({detail})")
                }
            }
            EventKind::Quarantined {
                number,
                email,
                reason,
            } => format!(
                "Account-{number} ({email}) quarantined: {reason}. Log in with it and run 'ccswap --add-account --slot {number}' to recover."
            ),
            EventKind::Unquarantined {
                number,
                email,
                reason,
            } => format!("Account-{number} ({email}) back in rotation ({reason})"),
            EventKind::AllExhausted { earliest_reset_at } => match earliest_reset_at {
                Some(at) if !at.is_empty() => {
                    format!("all accounts exhausted; earliest reset {at}")
                }
                _ => "all accounts exhausted; no reset time known".into(),
            },
            EventKind::Sleep { seconds, until } => {
                format!("sleeping {}m (until {until})", fixed0(seconds / 60.0))
            }
            EventKind::Error { message, transient } => {
                if *transient {
                    format!("error: {message} (will retry)")
                } else {
                    format!("error: {message}")
                }
            }
            EventKind::ConfigWarning { message } => format!("warning: {message}"),
        }
    }
}

fn poll_human(
    active: Option<&Value>,
    headroom: &[(String, Option<f64>)],
    threshold: f64,
    fetch_errors: &[(String, String)],
    windows: &[(String, Vec<(String, f64)>)],
) -> String {
    let Some(active) = active else {
        return "poll: no active account".into();
    };
    let num = py_str(&active["number"]);
    let lookup_h = |n: &str| headroom.iter().find(|(k, _)| k == n).and_then(|(_, h)| *h);
    let lookup_err = |n: &str| {
        fetch_errors
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, e)| e.as_str())
            .filter(|e| !e.is_empty())
    };
    let used = match lookup_h(&num) {
        Some(h) => format!("{}% used", fixed0(100.0 - h)),
        None => match lookup_err(&num) {
            Some(err) => format!("usage unknown ({err})"),
            None => "usage unknown".into(),
        },
    };
    let describe = |n: &str| {
        if let Some((_, wins)) = windows.iter().find(|(k, w)| k == n && !w.is_empty()) {
            return wins
                .iter()
                .map(|(name, pct)| format!("{name} {}%", fixed0(*pct)))
                .collect::<Vec<_>>()
                .join(" · ");
        }
        if let Some(h) = lookup_h(n) {
            return format!("{}%", fixed0(100.0 - h));
        }
        match lookup_err(n) {
            Some(err) => format!("? ({err})"),
            None => "?".into(),
        }
    };
    let others: Vec<String> = headroom
        .iter()
        .filter(|(n, _)| *n != num)
        .map(|(n, _)| format!("#{n}: {}", describe(n)))
        .collect();
    let tail = if others.is_empty() {
        String::new()
    } else {
        format!(" | others: {}", others.join(", "))
    };
    format!(
        "Account-{num} ({}): {used} (switch at {}%){tail}",
        py_str(&active["email"]),
        pct_label(threshold)
    )
}

/// Python `f"{v:.0f}"`.
pub(crate) fn fixed0(value: f64) -> String {
    let s = format!("{value:.0}");
    if s == "-0" { "0".into() } else { s }
}

fn py_round1(value: f64) -> f64 {
    let s = format!("{value:.1}");
    s.parse().unwrap_or(value)
}

/// Python `f"{v:.10g}"`: 99.9 never renders as 100, yet float noise in computed utilizations is absorbed.
pub fn pct_label(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".into()
        } else if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let sci = format!("{value:.9e}");
    let (mantissa, exp) = sci.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    if !(-4..10).contains(&exp) {
        let mantissa = strip_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let decimals = (9 - exp).max(0) as usize;
    strip_zeros(&format!("{value:.decimals$}")).to_string()
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_label_whole_numbers_drop_the_decimal() {
        assert_eq!(pct_label(90.0), "90");
    }

    #[test]
    fn pct_label_fractional_threshold_keeps_one_decimal() {
        assert_eq!(pct_label(99.9), "99.9");
    }

    #[test]
    fn pct_label_configured_precision_is_preserved() {
        assert_eq!(pct_label(85.55), "85.55");
        assert_eq!(pct_label(85.555555), "85.555555");
    }

    #[test]
    fn pct_label_float_noise_is_absorbed() {
        assert_eq!(pct_label(100.0 - 37.4), "62.6");
        assert_eq!(pct_label(99.850_000_000_000_01), "99.85");
    }

    #[test]
    fn pct_label_matches_percent_g_outside_the_positional_range() {
        assert_eq!(pct_label(0.0001), "0.0001");
        assert_eq!(pct_label(0.00001), "1e-05");
        assert_eq!(pct_label(12_345_678_901.0), "1.23456789e+10");
    }

    #[test]
    fn poll_event_shows_fractional_threshold() {
        let event = AutoSwitchEvent {
            ts: "1970-01-01T00:00:00Z".into(),
            kind: EventKind::Poll {
                active: Some(json!({"number": 1, "email": "a@example.com"})),
                headroom: vec![("1".into(), Some(40.0))],
                threshold: 99.9,
                fetch_errors: vec![],
                windows: vec![],
            },
        };
        assert!(event.human().contains("switch at 99.9%"));
    }
}
