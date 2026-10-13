//! Usage-aware target choice: the `best` pick and the `next-available`
//! skip rule, as pure functions of decision-grade usage.

use std::collections::HashMap;

use serde_json::Value;

use crate::usage::{account_headroom, relevant_windows};

/// Decision-grade usage per slot number; `None` when the slot's usage is
/// unknown (unavailable, stale or a sentinel).
pub type DecisionUsage = HashMap<String, Option<Value>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// Jump to the switchable account with provably the most headroom.
    Best,
    /// Rotate, skipping accounts known to be at their limit.
    NextAvailable,
}

impl Strategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "best" => Some(Strategy::Best),
            "next-available" => Some(Strategy::NextAvailable),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Strategy::Best => "best",
            Strategy::NextAvailable => "next-available",
        }
    }
}

/// The `best` verdict relative to the current account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BestPick {
    /// Strictly more headroom than the current account.
    Switch(String),
    /// The current account's usage is unknown, so nothing can be proven better.
    CurrentUnavailable,
    /// No candidate has known usage.
    NoComparison,
    /// Current is best among the measurable, but some candidate is unknown.
    IncompleteComparison,
    /// Current provably has the most headroom.
    Stay,
    /// Current is best and every account is at its limit.
    Exhausted,
    /// No other switchable account exists.
    NoCandidates,
}

fn headroom(usage: &DecisionUsage, num: &str, models: &[String]) -> Option<f64> {
    account_headroom(usage.get(num).and_then(Option::as_ref), models)
}

/// `candidates` are the other enabled, switchable slots in sequence order;
/// ties go to staying, then to the earliest candidate.
pub fn select_best(
    current: Option<&str>,
    candidates: &[String],
    usage: &DecisionUsage,
    models: &[String],
) -> BestPick {
    if candidates.is_empty() {
        return BestPick::NoCandidates;
    }
    let Some(current_headroom) = current.and_then(|c| headroom(usage, c, models)) else {
        return BestPick::CurrentUnavailable;
    };
    let scored: Vec<(Option<f64>, &String)> = candidates
        .iter()
        .map(|num| (headroom(usage, num, models), num))
        .collect();
    let mut best: Option<(f64, &String)> = None;
    for (h, num) in &scored {
        if let Some(h) = *h
            && best.is_none_or(|(b, _)| h > b)
        {
            best = Some((h, num));
        }
    }
    let Some((best_headroom, best_num)) = best else {
        return BestPick::NoComparison;
    };
    if best_headroom > current_headroom {
        return BestPick::Switch(best_num.clone());
    }
    if scored.iter().any(|(h, _)| h.is_none()) {
        return BestPick::IncompleteComparison;
    }
    if current_headroom <= 0.0 {
        return BestPick::Exhausted;
    }
    BestPick::Stay
}

/// `Some(label)` when `next-available` must skip the slot: its usage is
/// known and leaves no headroom. The label names the binding windows when
/// `models` are in play (`Fable`, `5h/Fable`), else `5h/7d`. Unknown usage
/// is never skipped.
pub fn at_limit_label(usage: Option<&Value>, models: &[String]) -> Option<String> {
    let headroom = account_headroom(usage, models)?;
    if headroom > 0.0 {
        return None;
    }
    let mut label = "5h/7d".to_string();
    if !models.is_empty() {
        let at: Vec<String> = relevant_windows(usage, models)
            .into_iter()
            .filter(|w| w.pct >= 100.0)
            .map(|w| w.label)
            .collect();
        if !at.is_empty() {
            label = at.join("/");
        }
    }
    Some(label)
}

/// Requested model names (not `all`) that no account's scoped windows
/// report. Claimed only when every account's usage is known: an unknown
/// account could be the one carrying the window.
pub fn inert_models(usage: &DecisionUsage, models: &[String]) -> Vec<String> {
    let mut wanted: Vec<(String, &String)> = Vec::new();
    for m in models {
        let low = m.to_lowercase();
        if low == "all" {
            continue;
        }
        match wanted.iter_mut().find(|(l, _)| *l == low) {
            Some(slot) => slot.1 = m,
            None => wanted.push((low, m)),
        }
    }
    if wanted.is_empty() || usage.is_empty() {
        return Vec::new();
    }
    let mut seen = Vec::new();
    for value in usage.values() {
        let Some(Value::Object(u)) = value else {
            return Vec::new();
        };
        if let Some(Value::Array(scoped)) = u.get("scoped") {
            seen.extend(
                scoped
                    .iter()
                    .filter_map(|s| s.get("name").and_then(Value::as_str))
                    .map(str::to_lowercase),
            );
        }
    }
    wanted
        .into_iter()
        .filter(|(low, _)| !seen.contains(low))
        .map(|(_, name)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pct(five_hour: f64) -> Option<Value> {
        Some(json!({"five_hour": {"pct": five_hour}, "seven_day": {"pct": 0.0}}))
    }

    fn model_pct(five_hour: f64, fable: f64) -> Option<Value> {
        Some(json!({
            "five_hour": {"pct": five_hour},
            "seven_day": {"pct": 0.0},
            "scoped": [{"name": "Fable", "pct": fable}],
        }))
    }

    fn usage(rows: &[(&str, Option<Value>)]) -> DecisionUsage {
        rows.iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect()
    }

    fn names(nums: &[&str]) -> Vec<String> {
        nums.iter().map(|s| s.to_string()).collect()
    }

    fn best(rows: &[(&str, Option<Value>)], models: &[&str]) -> BestPick {
        let others: Vec<&str> = rows.iter().map(|r| r.0).filter(|n| *n != "1").collect();
        select_best(Some("1"), &names(&others), &usage(rows), &names(models))
    }

    #[test]
    fn best_switches_to_more_headroom() {
        let rows = [("1", pct(50.0)), ("2", pct(90.0)), ("3", pct(20.0))];
        assert_eq!(best(&rows, &[]), BestPick::Switch("3".into()));
    }

    #[test]
    fn best_stays_when_current_is_already_best() {
        assert_eq!(
            best(&[("1", pct(89.0)), ("2", pct(100.0))], &[]),
            BestPick::Stay
        );
    }

    #[test]
    fn best_all_exhausted_stays_put() {
        let rows = [("1", pct(100.0)), ("2", pct(100.0)), ("3", pct(100.0))];
        assert_eq!(best(&rows, &[]), BestPick::Exhausted);
    }

    #[test]
    fn best_current_usage_unavailable_stays() {
        assert_eq!(
            best(&[("1", None), ("2", pct(10.0))], &[]),
            BestPick::CurrentUnavailable
        );
        assert_eq!(
            select_best(None, &names(&["2"]), &usage(&[("2", pct(10.0))]), &[]),
            BestPick::CurrentUnavailable
        );
    }

    #[test]
    fn best_no_candidate_usage_stays() {
        assert_eq!(
            best(&[("1", pct(50.0)), ("2", None)], &[]),
            BestPick::NoComparison
        );
    }

    #[test]
    fn best_incomplete_comparison_stays() {
        let rows = [("1", pct(50.0)), ("2", pct(90.0)), ("3", None)];
        assert_eq!(best(&rows, &[]), BestPick::IncompleteComparison);
    }

    #[test]
    fn best_current_exhausted_with_unknown_candidate_stays() {
        let rows = [("1", pct(100.0)), ("2", pct(100.0)), ("3", None)];
        assert_eq!(best(&rows, &[]), BestPick::IncompleteComparison);
    }

    #[test]
    fn best_strategy_ignores_disabled_candidate() {
        assert_eq!(
            select_best(Some("1"), &[], &usage(&[("1", pct(0.0))]), &[]),
            BestPick::NoCandidates
        );
    }

    #[test]
    fn best_ties_stay_and_equal_candidates_resolve_to_the_earliest() {
        assert_eq!(
            best(&[("1", pct(40.0)), ("2", pct(40.0))], &[]),
            BestPick::Stay
        );
        let rows = [("1", pct(90.0)), ("2", pct(10.0)), ("3", pct(10.0))];
        assert_eq!(best(&rows, &[]), BestPick::Switch("2".into()));
    }

    #[test]
    fn best_with_models_folds_scoped_into_the_comparison() {
        let rows = [
            ("1", model_pct(5.0, 90.0)),
            ("2", model_pct(5.0, 20.0)),
            ("3", model_pct(50.0, 80.0)),
        ];
        assert_eq!(best(&rows, &[]), BestPick::Stay);
        assert_eq!(best(&rows, &["Fable"]), BestPick::Switch("2".into()));
    }

    #[test]
    fn skip_exhausted_skips_limited_account_but_not_unknown() {
        assert_eq!(
            at_limit_label(pct(100.0).as_ref(), &[]),
            Some("5h/7d".into())
        );
        assert_eq!(at_limit_label(pct(20.0).as_ref(), &[]), None);
        assert_eq!(at_limit_label(None, &[]), None);
    }

    #[test]
    fn next_available_with_models_names_the_window() {
        let fable = names(&["Fable"]);
        assert_eq!(
            at_limit_label(model_pct(5.0, 100.0).as_ref(), &fable),
            Some("Fable".into())
        );
        assert_eq!(
            at_limit_label(model_pct(100.0, 100.0).as_ref(), &fable),
            Some("5h/Fable".into())
        );
    }

    #[test]
    fn next_available_without_models_ignores_scoped() {
        assert_eq!(at_limit_label(model_pct(5.0, 100.0).as_ref(), &[]), None);
    }

    #[test]
    fn manual_strategies_warn_on_inert_model_name() {
        let rows = usage(&[("1", model_pct(0.0, 10.0)), ("2", model_pct(5.0, 10.0))]);
        assert_eq!(inert_models(&rows, &names(&["Fabel"])), ["Fabel"]);
        assert!(inert_models(&rows, &names(&["Fable"])).is_empty());
        assert!(inert_models(&rows, &names(&["fable", "all"])).is_empty());
    }

    #[test]
    fn inert_models_stay_quiet_while_any_usage_is_unknown() {
        let rows = usage(&[("1", model_pct(0.0, 10.0)), ("2", None)]);
        assert!(inert_models(&rows, &names(&["Fabel"])).is_empty());
        assert!(inert_models(&DecisionUsage::new(), &names(&["Fabel"])).is_empty());
    }
}
