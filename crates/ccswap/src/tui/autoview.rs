//! The Auto screen's text (upstream `tui/autoview.py`): the decision-log
//! line for an engine event, the ranked "Next best" candidates and the
//! summary line.

use ccswap_core::autoswitch::{AutoSwitchEvent, AutoSwitchSettings, pct_label};
use ccswap_core::poll_policy::binding_pct;
use ccswap_core::snapshot::AccountsSnapshot;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::data;
use super::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Accent,
    Warn,
    Crit,
    Muted,
    Foreground,
}

impl Role {
    pub fn color(self, palette: &Palette) -> Color {
        match self {
            Role::Accent => palette.accent,
            Role::Warn => palette.sev_warn,
            Role::Crit => palette.sev_crit,
            Role::Muted => palette.muted,
            Role::Foreground => palette.foreground,
        }
    }
}

/// One decision-log entry; the colour resolves at draw time so a theme
/// change restyles the whole log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub stamp: Option<String>,
    pub text: String,
    pub role: Role,
}

impl LogLine {
    pub fn note(text: impl Into<String>) -> Self {
        LogLine {
            stamp: None,
            text: text.into(),
            role: Role::Muted,
        }
    }

    pub fn line(&self, palette: &Palette) -> Line<'static> {
        let mut spans = Vec::new();
        if let Some(stamp) = &self.stamp {
            spans.push(Span::styled(
                format!("{stamp}  "),
                Style::default().fg(palette.muted),
            ));
        }
        spans.push(Span::styled(
            self.text.clone(),
            Style::default().fg(self.role.color(palette)),
        ));
        Line::from(spans)
    }

    pub fn plain(&self) -> String {
        match &self.stamp {
            Some(stamp) => format!("{stamp}  {}", self.text),
            None => self.text.clone(),
        }
    }
}

pub fn event_role(event: &AutoSwitchEvent) -> Role {
    match event.name() {
        "switch" => Role::Accent,
        "error" | "account-quarantined" => Role::Warn,
        "all-exhausted" => Role::Crit,
        "poll" | "no-switch" | "sleep" | "account-unquarantined" => Role::Muted,
        _ => Role::Foreground,
    }
}

pub fn event_line(event: &AutoSwitchEvent, stamp: String) -> LogLine {
    LogLine {
        stamp: Some(stamp),
        text: event.human(),
        role: event_role(event),
    }
}

pub fn clock_stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Switch targets by binding usage, best first, over the engine's window
/// set; sentinel and unknown accounts sink to the bottom.
pub fn candidate_lines(
    snap: &AccountsSnapshot,
    models: &[String],
    palette: &Palette,
) -> Vec<Line<'static>> {
    let fg = |c| Style::default().fg(c);
    let mut ranked: Vec<(f64, String, Line<'static>)> = Vec::new();
    for acc in &snap.accounts {
        if snap.active_number.as_ref() == Some(&acc.number) || !acc.switchable {
            continue;
        }
        let mut spans = vec![
            Span::styled(format!("  {:>2}  ", acc.number), fg(palette.foreground)),
            Span::styled(acc.email.clone(), fg(palette.foreground)),
        ];
        let key = if let Some(sentinel) = acc.usage.sentinel {
            spans.push(Span::styled(
                format!("  {}", data::sentinel_label(sentinel)),
                fg(palette.muted),
            ));
            998.0
        } else if let Some(pct) = binding_pct(acc.usage.last_good.as_ref(), models) {
            spans.push(Span::styled(
                format!("  {pct:3.0}% used"),
                fg(palette.severity(Some(pct))),
            ));
            pct
        } else {
            spans.push(Span::styled("  usage unknown", fg(palette.muted)));
            999.0
        };
        ranked.push((key, acc.number.clone(), Line::from(spans)));
    }
    let mut lines = vec![Line::styled("Next best", fg(palette.muted))];
    if ranked.is_empty() {
        lines.push(Line::styled(
            "  no other switchable accounts",
            fg(palette.muted),
        ));
        return lines;
    }
    // Slot numbers compare as strings on ties, like upstream.
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    lines.extend(ranked.into_iter().map(|(_, _, line)| line));
    lines
}

pub fn summary_spans(
    settings: &AutoSwitchSettings,
    configured_threshold: f64,
    adjusting: bool,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let muted = Style::default().fg(palette.muted);
    let mut spans = vec![
        Span::styled("auto-switch · ", muted),
        Span::styled(
            format!("threshold {}%", pct_label(settings.threshold)),
            if adjusting {
                Style::default().fg(palette.accent)
            } else {
                muted
            },
        ),
    ];
    if settings.threshold != configured_threshold {
        spans.push(Span::styled(" (session)", muted));
    }
    spans.push(Span::styled(
        format!(" · poll every {:.0}s", settings.interval_seconds),
        muted,
    ));
    if adjusting {
        spans.push(Span::styled("   ← → adjust · enter done", muted));
    }
    spans
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::TestEventText.
    use super::*;
    use crate::tui::theme::{DARK, LIGHT};
    use ccswap_core::autoswitch::{EventKind, Trigger};
    use serde_json::json;

    fn switch_event() -> AutoSwitchEvent {
        AutoSwitchEvent {
            ts: "2026-01-01T00:00:00Z".into(),
            kind: EventKind::Switch {
                trigger: Trigger::Proactive,
                from: Some(json!({"number": 1, "email": "a@x.com"})),
                to: Some(json!({"number": 2, "email": "b@x.com"})),
                warnings: Vec::new(),
                dry_run: false,
            },
        }
    }

    #[test]
    fn switch_event_styling_and_content() {
        let event = switch_event();
        let line = event_line(&event, "12:34:56".into());
        assert_eq!(line.plain(), format!("12:34:56  {}", event.human()));
        assert_eq!(line.role, Role::Accent);
    }

    #[test]
    fn event_text_uses_light_accent_for_switch() {
        let line = event_line(&switch_event(), "12:34:56".into()).line(&LIGHT);
        assert!(line.spans.iter().any(|s| s.style.fg == Some(LIGHT.accent)));
        let dark = event_line(&switch_event(), "12:34:56".into()).line(&DARK);
        assert!(!dark.spans.iter().any(|s| s.style.fg == Some(LIGHT.accent)));
    }

    #[test]
    fn quiet_and_alarming_kinds() {
        let event = |kind| AutoSwitchEvent {
            ts: String::new(),
            kind,
        };
        let no_switch = event(EventKind::NoSwitch {
            reason: "cooldown",
            detail: String::new(),
        });
        assert_eq!(event_role(&no_switch), Role::Muted);
        let exhausted = event(EventKind::AllExhausted {
            earliest_reset_at: None,
        });
        assert_eq!(event_role(&exhausted), Role::Crit);
        let error = event(EventKind::Error {
            message: "x".into(),
            transient: false,
        });
        assert_eq!(event_role(&error), Role::Warn);
        let warning = event(EventKind::ConfigWarning {
            message: "x".into(),
        });
        assert_eq!(event_role(&warning), Role::Foreground);
    }
}
