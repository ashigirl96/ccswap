//! The usage-aware strategies of bare `switch`: `best` and the
//! `next-available` skip rule on top of the rotation walk.

use serde_json::Value;

use super::{AccountRef, SwitchReport, Switcher, is_disabled};
use crate::console::Console;
use crate::error::Result;
use crate::pyjson::py_str;
use crate::ranking::{self, BestPick, DecisionUsage, Strategy};
use crate::store::JsonObject;
use crate::usage_collector::{UsageCollector, build_accounts_info};
use crate::usage_store::DecisionValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// `--model` on the command line.
    Cli,
    /// The `autoswitch.model` setting.
    Setting,
}

impl ModelSource {
    /// The `modelSource` value of `switch --json`.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Cli => "cli",
            ModelSource::Setting => "autoswitch.model",
        }
    }

    fn announced(self) -> &'static str {
        match self {
            ModelSource::Cli => "--model",
            ModelSource::Setting => "autoswitch.model",
        }
    }
}

/// A usage-aware bare `switch`. `models` fold those per-model weekly
/// windows into every headroom comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchStrategy {
    pub strategy: Strategy,
    pub models: Vec<String>,
    pub model_source: Option<ModelSource>,
}

impl SwitchStrategy {
    pub fn new(strategy: Strategy) -> Self {
        SwitchStrategy {
            strategy,
            models: Vec::new(),
            model_source: None,
        }
    }
}

/// Where a strategy left the rotation walk.
pub(super) enum Hook {
    Done(Option<SwitchReport>),
    Rotate,
}

impl Switcher<'_> {
    pub(super) fn announce_models(
        &self,
        strategy: Option<&SwitchStrategy>,
        json: bool,
        console: &mut dyn Console,
    ) {
        let Some(s) = strategy.filter(|s| !s.models.is_empty() && !json) else {
            return;
        };
        let source = s
            .model_source
            .map(|m| format!(" (from {})", m.announced()))
            .unwrap_or_default();
        console.line(&self.style.dimmed(&format!(
            "Using configured model limits: {}{source}",
            s.models.join(", ")
        )));
    }

    /// Decision-grade usage of every managed account; never fails on the
    /// network.
    pub(super) fn decision_usage(&self, data: &JsonObject) -> DecisionUsage {
        let (infos, active) = build_accounts_info(self.paths, self.secrets, data);
        UsageCollector::new(self.paths, self.env, self.secrets, self.client)
            .collect(&infos, active.as_ref())
            .into_iter()
            .map(|(num, entry)| {
                let value = match entry.decision_value() {
                    DecisionValue::Usage(v) => Some(v.clone()),
                    _ => None,
                };
                (num, value)
            })
            .collect()
    }

    pub(super) fn warn_inert_models(
        &self,
        usage: &DecisionUsage,
        models: &[String],
        json: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) {
        let missing = ranking::inert_models(usage, models);
        if missing.is_empty() {
            return;
        }
        let message = format!(
            "model(s) {} match no account's usage windows (typo?)",
            missing.join(", ")
        );
        if json {
            warnings.push(message);
        } else {
            console.line(&self.style.yellowed(&message));
        }
    }

    /// `best`: switch only onto provably more headroom, else stay with a
    /// reason; without any other candidate the rotation reports it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn switch_best(
        &self,
        data: &JsonObject,
        current: &str,
        current_ref: &AccountRef,
        models: &[String],
        json: bool,
        warnings: &mut Vec<String>,
        console: &mut dyn Console,
    ) -> Result<Hook> {
        const LABEL: &str = "best";
        let usage = self.decision_usage(data);
        self.warn_inert_models(&usage, models, json, warnings, console);
        let candidates: Vec<String> = data
            .get("sequence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(py_str)
            .filter(|n| n != current && self.is_switchable(data, n) && !is_disabled(data, n))
            .collect();
        let style = self.style;
        let (reason, json_message, human): (&'static str, String, String) =
            match ranking::select_best(Some(current), &candidates, &usage, models) {
                BestPick::Switch(target) => {
                    let performed = self.perform(&target, !json, false, None, console)?;
                    return Ok(Hook::Done(json.then(|| {
                        SwitchReport::from_performed(performed, LABEL, std::mem::take(warnings))
                    })));
                }
                BestPick::NoCandidates => return Ok(Hook::Rotate),
                BestPick::CurrentUnavailable => {
                    let m = format!(
                        "Current account usage is unavailable — staying on Account-{current}."
                    );
                    let h = style.dimmed(&format!("{m} Run ccswap --switch to rotate."));
                    ("usage-unavailable", m, h)
                }
                BestPick::NoComparison => {
                    let m = format!(
                        "No other account has usage data to compare — staying on Account-{current}."
                    );
                    let h = style.dimmed(&format!("{m} Run ccswap --switch to rotate."));
                    ("usage-unavailable", m, h)
                }
                BestPick::IncompleteComparison => {
                    let m = format!(
                        "No account with known usage has more remaining quota; some usage is unavailable — staying on Account-{current}."
                    );
                    let h = style.dimmed(&m);
                    ("usage-unavailable", m, h)
                }
                BestPick::Stay => {
                    let m = format!(
                        "Already on the account with the most remaining quota (Account-{current})."
                    );
                    let h = format!(
                        "{} (Account-{current}).",
                        style.accent("Already on the account with the most remaining quota")
                    );
                    ("already-best", m, h)
                }
                BestPick::Exhausted => {
                    let m = format!(
                        "All accounts are at their {} — staying on Account-{current}.",
                        limits_label(models)
                    );
                    let h = style.yellowed(&m);
                    ("candidates-exhausted", m, h)
                }
            };
        if json {
            return Ok(Hook::Done(Some(SwitchReport::noop(
                LABEL,
                reason,
                Some(current_ref.clone()),
                json_message,
                std::mem::take(warnings),
            ))));
        }
        console.line(&human);
        Ok(Hook::Done(None))
    }
}

/// With model limits in play the binding window may be a scoped one.
pub(super) fn limits_label(models: &[String]) -> &'static str {
    if models.is_empty() {
        "5h/7d limit"
    } else {
        "usage limits"
    }
}
