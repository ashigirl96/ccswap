use ccswap_claude::snapshot::{AccountSnapshot, AccountsSnapshot};
use ccswap_core::usage::{Sentinel, error_note, truthy};
use ccswap_core::usage_store::STALE_OK_S;
use ccswap_core::usage_view::last_seen_note;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::data;
use super::theme::Palette;

pub const BAR_FILLED: &str = "━";
pub const BAR_HALF: &str = "╸";
pub const BAR_EMPTY: &str = "─";
pub const BAR_TICK: &str = "┃";

fn fg(color: ratatui::style::Color) -> Style {
    Style::default().fg(color)
}

fn bold(color: ratatui::style::Color) -> Style {
    fg(color).add_modifier(Modifier::BOLD)
}

fn dim_if(style: Style, stale: bool) -> Style {
    if stale {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

/// Python's `round()`: ties go to the even neighbour.
fn round_half_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 {
        (r - x.signum()) as i64
    } else {
        r as i64
    }
}

pub fn bar_cells(
    pct: Option<f64>,
    width: usize,
    stale: bool,
    threshold: Option<f64>,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let Some(pct) = pct else {
        return vec![Span::styled(BAR_EMPTY.repeat(width), fg(palette.track))];
    };
    let cells = pct.clamp(0.0, 100.0) / 100.0 * width as f64;
    let full = cells as usize;
    let half = cells - full as f64 >= 0.5 && full < width;
    let tick_at = threshold
        .map(|t| round_half_even(t / 100.0 * width as f64).clamp(0, width as i64 - 1) as usize);
    let fill = dim_if(fg(palette.severity(Some(pct))), stale);
    (0..width)
        .map(|i| {
            if tick_at == Some(i) {
                Span::styled(BAR_TICK, fg(palette.sev_warn))
            } else if i < full {
                Span::styled(BAR_FILLED, fill)
            } else if i == full && half {
                Span::styled(BAR_HALF, fill)
            } else {
                Span::styled(BAR_EMPTY, fg(palette.track))
            }
        })
        .collect()
}

pub fn usage_bar(
    label: &str,
    pct: Option<f64>,
    suffix: Option<&str>,
    width: usize,
    stale: bool,
    threshold: Option<f64>,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(format!("{label} "), fg(palette.muted))];
    spans.extend(bar_cells(pct, width, stale, threshold, palette));
    match pct {
        None => spans.push(Span::styled("  usage unknown", fg(palette.muted))),
        Some(p) => spans.push(Span::styled(
            format!(" {p:3.0}%"),
            dim_if(fg(palette.severity(Some(p))), stale),
        )),
    }
    if let Some(suffix) = suffix.filter(|s| !s.is_empty()) {
        spans.push(Span::styled(format!("  {suffix}"), fg(palette.muted)));
    }
    spans
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub label: String,
    pub pct: f64,
    pub suffix: String,
    pub suffix_full: String,
}

/// `f"{x:,.2f}"`.
fn money(x: f64) -> String {
    let fixed = format!("{:.2}", x.abs());
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    format!("{sign}{grouped}.{frac}")
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        other => other.as_f64(),
    }
}

fn reset_parts(window: &Value, now: f64) -> (String, String) {
    let Some(reset) = data::reset_text(Some(window), now) else {
        return (String::new(), String::new());
    };
    let full = match data::reset_clock(Some(window), now) {
        Some(clock) => format!("{reset} · {clock}"),
        None => reset.clone(),
    };
    (reset, full)
}

fn append(suffix: &mut String, marker: &str) {
    if suffix.is_empty() {
        suffix.push_str(marker);
    } else {
        suffix.push_str("  ");
        suffix.push_str(marker);
    }
}

pub fn usage_rows(last_good: Option<&Value>, now: f64, fetched_at: Option<f64>) -> Vec<UsageRow> {
    let Some(last_good) = last_good.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    if let Some(spend) = last_good.get("spend").filter(|v| truthy(v))
        && let Some(pct) = number(spend.get("pct"))
    {
        let amounts = format!(
            "${} / ${}",
            money(number(spend.get("used")).unwrap_or(0.0)),
            money(number(spend.get("limit")).unwrap_or(0.0))
        );
        let (reset, reset_full) = reset_parts(spend, now);
        let join = |r: &str| {
            if r.is_empty() {
                amounts.clone()
            } else {
                format!("{r}  {amounts}")
            }
        };
        rows.push(UsageRow {
            label: "$$".into(),
            pct,
            suffix: join(&reset),
            suffix_full: join(&reset_full),
        });
    }
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        let Some(window) = last_good.get(key).filter(|v| truthy(v)) else {
            continue;
        };
        let Some(pct) = number(window.get("pct")) else {
            continue;
        };
        let (mut suffix, mut suffix_full) = reset_parts(window, now);
        if key == "seven_day" && data::ahead_of_pace(window, fetched_at) {
            append(&mut suffix, "(ahead of pace)");
            append(&mut suffix_full, "(ahead of pace)");
        }
        rows.push(UsageRow {
            label: label.into(),
            pct,
            suffix,
            suffix_full,
        });
    }
    let scoped = last_good.get("scoped").and_then(Value::as_array);
    for window in scoped.into_iter().flatten() {
        let Some(pct) = number(window.get("pct")) else {
            continue;
        };
        let (mut suffix, mut suffix_full) = reset_parts(window, now);
        let marker = if pct >= 100.0 {
            Some("(!)")
        } else if data::ahead_of_pace(window, fetched_at) {
            Some("(ahead of pace)")
        } else {
            None
        };
        if let Some(marker) = marker {
            append(&mut suffix, marker);
            append(&mut suffix_full, marker);
        }
        let name = match window.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        rows.push(UsageRow {
            label: name,
            pct,
            suffix,
            suffix_full,
        });
    }
    rows
}

fn name_spans(acc: &AccountSnapshot, palette: &Palette) -> Vec<Span<'static>> {
    if acc.alias.is_empty() {
        vec![Span::styled(acc.email.clone(), fg(palette.foreground))]
    } else {
        vec![
            Span::styled(acc.alias.clone(), bold(palette.accent)),
            Span::styled(format!(" ({})", acc.email), fg(palette.foreground)),
        ]
    }
}

fn is_stale(acc: &AccountSnapshot) -> bool {
    acc.usage.age_s.is_some_and(|a| a > STALE_OK_S)
}

pub fn account_card(
    acc: &AccountSnapshot,
    width: usize,
    threshold: Option<f64>,
    now: f64,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut header = vec![Span::styled(
        format!("{:>2}  ", acc.number),
        bold(palette.foreground),
    )];
    header.extend(name_spans(acc, palette));
    header.push(Span::styled(
        format!("  [{}]", acc.display_tag()),
        fg(palette.muted),
    ));
    if acc.is_active {
        header.push(Span::styled("   ● active", bold(palette.accent)));
    }
    if acc.disabled {
        header.push(Span::styled("   (disabled)", fg(palette.muted)));
    }
    if let Some(age) = data::format_age(acc.usage.age_s) {
        header.push(Span::styled(format!("   {age}"), fg(palette.muted)));
    }
    let mut lines = vec![Line::from(header)];
    let indent = || Span::raw("    ");

    if let Some(sentinel) = acc.usage.sentinel {
        let api_key = sentinel == Sentinel::ApiKey;
        let (marker, color) = if api_key {
            ("·", palette.muted)
        } else {
            ("⚠", palette.sev_warn)
        };
        lines.push(Line::from(vec![
            indent(),
            Span::styled(
                format!("{marker} {}", data::sentinel_label(sentinel)),
                fg(color),
            ),
        ]));
        if !api_key && let Some(seen) = last_seen_note(&acc.usage) {
            lines.push(Line::from(vec![
                indent(),
                Span::styled(format!("└ {seen}"), fg(palette.muted)),
            ]));
        }
        return lines;
    }

    let rows = usage_rows(acc.usage.last_good.as_ref(), now, acc.usage.fetched_at);
    if rows.is_empty() {
        let mut spans = vec![
            indent(),
            Span::styled("usage unavailable", fg(palette.muted)),
        ];
        if let Some(err) = acc.usage.last_error.as_deref().filter(|e| !e.is_empty()) {
            spans.push(Span::styled(
                format!(" · {}", error_note(err).unwrap_or(err)),
                fg(palette.muted),
            ));
        }
        lines.push(Line::from(spans));
        return lines;
    }

    let stale = is_stale(acc);
    let label_width = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0);
    let bar_width = (width as i64 - 42 - label_width as i64).clamp(12, 30) as usize;
    let row_overhead = 4 + label_width + 1 + bar_width + 5 + 2;
    for row in rows {
        let suffix = if row.suffix_full != row.suffix
            && row_overhead + row.suffix_full.chars().count() <= width
        {
            &row.suffix_full
        } else {
            &row.suffix
        };
        let mut spans = vec![indent()];
        spans.extend(usage_bar(
            &format!("{:<label_width$}", row.label),
            Some(row.pct),
            Some(suffix),
            bar_width,
            stale,
            threshold,
            palette,
        ));
        lines.push(Line::from(spans));
    }
    lines
}

pub fn mini_account(acc: &AccountSnapshot, now: f64, palette: &Palette) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{:>2}  ", acc.number),
        bold(palette.muted),
    )];
    spans.extend(name_spans(acc, palette));
    spans.push(Span::styled(
        format!("  [{}]", acc.display_tag()),
        fg(palette.muted),
    ));
    if acc.disabled {
        spans.push(Span::styled("  (disabled)", fg(palette.muted)));
    }
    spans.push(Span::raw("   "));

    if let Some(sentinel) = acc.usage.sentinel {
        let color = if sentinel == Sentinel::ApiKey {
            palette.muted
        } else {
            palette.sev_warn
        };
        spans.push(Span::styled(data::sentinel_label(sentinel), fg(color)));
        return Line::from(spans);
    }

    let last_good = acc.usage.last_good.as_ref().and_then(Value::as_object);
    let stale = is_stale(acc);
    let mut parts = 0;
    for (key, label) in [("five_hour", "5h"), ("seven_day", "7d")] {
        let Some(window) = last_good.and_then(|l| l.get(key)).filter(|v| truthy(v)) else {
            continue;
        };
        let Some(pct) = number(window.get("pct")) else {
            continue;
        };
        if parts > 0 {
            spans.push(Span::styled(" · ", fg(palette.track)));
        }
        spans.push(Span::styled(format!("{label} "), fg(palette.muted)));
        spans.push(Span::styled(
            format!("{pct:.0}%"),
            dim_if(fg(palette.severity(Some(pct))), stale),
        ));
        if pct >= 100.0 {
            if let Some(reset) = data::reset_text(Some(window), now) {
                spans.push(Span::styled(format!(" ({reset})"), fg(palette.muted)));
            }
        } else if key == "seven_day" && data::ahead_of_pace(window, acc.usage.fetched_at) {
            spans.push(Span::styled(" (ahead)", fg(palette.sev_warn)));
        }
        parts += 1;
    }
    let scoped = last_good
        .and_then(|l| l.get("scoped"))
        .and_then(Value::as_array);
    for window in scoped.into_iter().flatten() {
        if number(window.get("pct")).is_some_and(|p| p >= 100.0) {
            if parts > 0 {
                spans.push(Span::styled(" · ", fg(palette.track)));
            }
            let name = window.get("name").and_then(Value::as_str).unwrap_or("");
            spans.push(Span::styled(format!("{name} (!)"), fg(palette.sev_crit)));
            parts += 1;
        }
    }
    if parts == 0 {
        spans.push(Span::styled("usage unknown", fg(palette.muted)));
    }
    Line::from(spans)
}

pub fn accounts_panel(
    snapshot: Option<&AccountsSnapshot>,
    width: usize,
    threshold: Option<f64>,
    show_minis: bool,
    now: f64,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let muted = fg(palette.muted);
    let Some(snap) = snapshot else {
        return vec![Line::styled("loading…", muted)];
    };
    if snap.accounts.is_empty() {
        return wrap(
            "No managed accounts yet.\nUse the menu below: Add account — from your current Claude Code login, or from a setup-token / API key.",
            width,
        )
        .into_iter()
        .map(|l| Line::styled(l, muted))
        .collect();
    }
    let blocks: Vec<Vec<Line<'static>>> = snap
        .accounts
        .iter()
        .filter_map(|acc| {
            if acc.is_active {
                Some(account_card(acc, width, threshold, now, palette))
            } else if show_minis {
                Some(vec![ellipsize(mini_account(acc, now, palette), width)])
            } else {
                None
            }
        })
        .collect();
    if blocks.is_empty() {
        return vec![Line::styled("no active managed login", muted)];
    }
    let mut lines = Vec::new();
    let mut previous_multiline = false;
    for (i, block) in blocks.into_iter().enumerate() {
        let multiline = block.len() > 1;
        if i > 0 && (multiline || previous_multiline) {
            lines.push(Line::default());
        }
        lines.extend(block);
        previous_multiline = multiline;
    }
    lines
}

/// Greedy word wrap that keeps explicit line breaks.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if candidate.chars().count() <= width || line.is_empty() {
                line = candidate;
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        out.push(line);
    }
    out
}

/// Cuts a line to `width` cells, ending in `…` when it overflows.
pub fn ellipsize(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width || width == 0 {
        return line;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut text = String::new();
        for c in span.content.chars() {
            if used + 1 >= width {
                break;
            }
            text.push(c);
            used += 1;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if used + 1 >= width {
            out.push(Span::styled("…", span.style));
            break;
        }
    }
    Line::from(out)
}

#[cfg(test)]
mod tests {
    //! Ported from upstream tests/test_tui.py::{TestUsageRows,
    //! TestMiniAccountText, TestFormatting card cases}.
    use super::*;
    use crate::tui::testing::{entry, iso_in, make_account, plain_lines};
    use crate::tui::theme::{DARK, LIGHT};
    use ccswap_core::usage::now_secs;
    use ccswap_core::usage_store::UsageEntry;
    use serde_json::json;

    fn card(acc: &AccountSnapshot, width: usize) -> String {
        plain_lines(&account_card(acc, width, None, now_secs(), &DARK))
    }

    fn labels(rows: &[UsageRow]) -> Vec<&str> {
        rows.iter().map(|r| r.label.as_str()).collect()
    }

    #[test]
    fn bar_fill_half_and_tick() {
        let text = |spans: Vec<Span>| {
            spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(
            text(bar_cells(Some(50.0), 10, false, None, &DARK)),
            "━━━━━─────"
        );
        assert_eq!(
            text(bar_cells(Some(55.0), 10, false, None, &DARK)),
            "━━━━━╸────"
        );
        assert_eq!(text(bar_cells(None, 4, false, None, &DARK)), "────");
        assert_eq!(text(bar_cells(Some(150.0), 4, false, None, &DARK)), "━━━━");
        let ticked = bar_cells(Some(100.0), 10, false, Some(90.0), &DARK);
        assert_eq!(text(ticked.clone()), "━━━━━━━━━┃");
        assert_eq!(ticked[9].style.fg, Some(DARK.sev_warn));
        // round(0.25 * 10) == 2 in Python (ties to even)
        assert_eq!(
            text(bar_cells(Some(0.0), 10, false, Some(25.0), &DARK)),
            "──┃───────"
        );
        assert_eq!(
            text(bar_cells(Some(0.0), 10, false, Some(35.0), &DARK)),
            "────┃─────"
        );
    }

    #[test]
    fn bar_colour_follows_severity() {
        let colour = |pct| bar_cells(Some(pct), 10, false, None, &DARK)[0].style.fg;
        assert_eq!(colour(69.0), Some(DARK.sev_ok));
        assert_eq!(colour(70.0), Some(DARK.sev_warn));
        assert_eq!(colour(90.0), Some(DARK.sev_crit));
    }

    #[test]
    fn absent_window_produces_no_row() {
        let e = entry(Some(47.0), None);
        assert_eq!(
            labels(&usage_rows(e.last_good.as_ref(), now_secs(), None)),
            ["5h"]
        );
    }

    #[test]
    fn scoped_models_and_over_limit_marker() {
        let mut e = entry(Some(25.0), Some(10.0));
        e.last_good.as_mut().unwrap()["scoped"] = json!([
            {"name": "Fable", "pct": 100.0, "resets_at": iso_in(86400.0 * 2.0)},
            {"name": "Opus", "pct": 12.0, "resets_at": iso_in(86400.0 * 2.0)},
        ]);
        let rows = usage_rows(e.last_good.as_ref(), now_secs(), None);
        assert_eq!(labels(&rows), ["5h", "7d", "Fable", "Opus"]);
        let fable = &rows[2];
        assert!(fable.suffix.contains("(!)"));
        assert!(fable.suffix_full.ends_with("(!)") && fable.suffix_full.contains(" · "));
    }

    fn spend(resets: bool) -> Value {
        let mut s = json!({"used": 12.5, "limit": 50.0, "pct": 25.0, "currency": "USD"});
        if resets {
            s["resets_at"] = iso_in(7200.0).into();
        }
        s
    }

    #[test]
    fn spend_row_first_with_amounts() {
        let mut e = entry(Some(25.0), Some(10.0));
        e.last_good.as_mut().unwrap()["spend"] = spend(false);
        let rows = usage_rows(e.last_good.as_ref(), now_secs(), None);
        assert_eq!(rows[0].label, "$$");
        assert!(rows[0].suffix.contains("$12.50 / $50.00"));
    }

    #[test]
    fn suffix_full_extends_countdown_with_clock() {
        let e = entry(Some(47.0), Some(10.0));
        let row = &usage_rows(e.last_good.as_ref(), now_secs(), None)[0];
        assert!(row.suffix.starts_with("resets "));
        assert!(row.suffix_full.starts_with(&format!("{} · ", row.suffix)));
    }

    #[test]
    fn spend_clock_sits_with_reset_not_after_amounts() {
        let mut e = entry(Some(25.0), Some(10.0));
        e.last_good.as_mut().unwrap()["spend"] = spend(true);
        let row = &usage_rows(e.last_good.as_ref(), now_secs(), None)[0];
        assert_eq!(row.label, "$$");
        let full = &row.suffix_full;
        assert!(full.find(" · ").unwrap() < full.find("$12.50").unwrap());
    }

    #[test]
    fn no_data_no_rows() {
        assert!(usage_rows(None, now_secs(), None).is_empty());
        assert!(usage_rows(Some(&json!({})), now_secs(), None).is_empty());
    }

    #[test]
    fn pace_markers_on_weekly_rows_only() {
        let now = now_secs();
        let week = json!({"seven_day": {"pct": 50.0, "resets_at": iso_in(86400.0 * 6.0)}});
        let row = &usage_rows(Some(&week), now, Some(now))[0];
        assert!(
            row.suffix.contains("(ahead of pace)") && row.suffix_full.contains("(ahead of pace)")
        );
        assert!(
            !usage_rows(Some(&week), now, None)[0]
                .suffix
                .contains("pace")
        );

        let five = json!({"five_hour": {"pct": 90.0, "resets_at": iso_in(3600.0 * 4.0)}});
        assert!(
            !usage_rows(Some(&five), now, Some(now))[0]
                .suffix
                .contains("pace")
        );

        let scoped =
            json!({"scoped": [{"name": "Fable", "pct": 50.0, "resets_at": iso_in(86400.0 * 6.0)}]});
        assert!(
            usage_rows(Some(&scoped), now, Some(now))[0]
                .suffix
                .contains("(ahead of pace)")
        );

        let maxed = json!({"scoped": [{"name": "Fable", "pct": 100.0, "resets_at": iso_in(86400.0 * 6.0)}]});
        let row = &usage_rows(Some(&maxed), now, Some(now))[0];
        assert!(row.suffix.contains("(!)") && !row.suffix.contains("ahead of pace"));
    }

    #[test]
    fn card_shows_clock_only_where_it_fits() {
        let mut e = entry(Some(25.0), Some(10.0));
        e.last_good.as_mut().unwrap()["spend"] = spend(true);
        let acc = make_account("1", true, e);

        assert_eq!(card(&acc, 100).matches(" · ").count(), 3);

        let mid = card(&acc, 78);
        let spend_line = mid.lines().find(|l| l.contains("$12.50")).unwrap();
        assert!(!spend_line.contains(" · "));
        for line in mid
            .lines()
            .filter(|l| l.contains("resets") && !l.contains("$12.50"))
        {
            assert!(line.contains(" · "));
        }

        assert!(!card(&acc, 40).contains(" · "));
    }

    #[test]
    fn sentinel_card_shows_last_seen_like_list() {
        let expired = UsageEntry {
            sentinel: Some(Sentinel::TokenExpired),
            last_good: Some(json!({"five_hour": {"pct": 53.0}})),
            fetched_at: Some(now_secs() - 720.0),
            age_s: Some(720.0),
            ..Default::default()
        };
        let text = card(&make_account("1", true, expired.clone()), 80);
        assert!(text.contains("token expired — refresh deferred this pass; retries automatically"));
        assert!(text.contains("last seen 53% used"));

        let bare = UsageEntry {
            sentinel: Some(Sentinel::TokenExpired),
            ..Default::default()
        };
        assert!(!card(&make_account("1", false, bare), 80).contains("last seen"));

        let api_key = UsageEntry {
            sentinel: Some(Sentinel::ApiKey),
            ..expired
        };
        assert!(!card(&make_account("1", false, api_key), 80).contains("last seen"));
    }

    #[test]
    fn card_uses_light_palette_when_passed() {
        let acc = make_account("1", true, entry(Some(95.0), Some(10.0)));
        let lines = account_card(&acc, 100, None, now_secs(), &LIGHT);
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains("● active") && s.style.fg == Some(LIGHT.accent))
        );
    }

    #[test]
    fn unavailable_card_names_the_error() {
        let e = UsageEntry {
            last_error: Some("consume-busy".into()),
            ..Default::default()
        };
        let text = card(&make_account("1", true, e), 80);
        assert!(text.contains(
            "usage unavailable · another cswap surface holds the slot — retries next pass"
        ));
    }

    fn mini(e: UsageEntry) -> String {
        let line = mini_account(&make_account("1", false, e), now_secs(), &DARK);
        plain_lines(&[line])
    }

    #[test]
    fn mini_pace_marker_only_on_seven_day_with_fetched_at() {
        let now = now_secs();
        let week = |fetched_at| UsageEntry {
            last_good: Some(
                json!({"seven_day": {"pct": 50.0, "resets_at": iso_in(86400.0 * 6.0)}}),
            ),
            fetched_at,
            age_s: fetched_at.map(|_| 0.0),
            ..Default::default()
        };
        assert!(mini(week(Some(now))).contains("(ahead)"));
        assert!(!mini(week(None)).contains("pace") && !mini(week(None)).contains("ahead"));
        let five = UsageEntry {
            last_good: Some(json!({"five_hour": {"pct": 90.0, "resets_at": iso_in(3600.0 * 4.0)}})),
            fetched_at: Some(now),
            age_s: Some(0.0),
            ..Default::default()
        };
        assert!(!mini(five).contains("ahead"));
    }

    #[test]
    fn mini_maxed_windows_bring_their_reset_and_scoped_mark() {
        let e = UsageEntry {
            last_good: Some(json!({
                "five_hour": {"pct": 100.0, "resets_at": iso_in(3600.0)},
                "seven_day": {"pct": 63.0},
                "scoped": [{"name": "Fable", "pct": 100.0}, {"name": "Opus", "pct": 3.0}],
            })),
            fetched_at: Some(now_secs()),
            age_s: Some(0.0),
            ..Default::default()
        };
        let text = mini(e);
        assert!(text.contains("5h 100% (resets ") && text.contains(" · 7d 63% · Fable (!)"));
        assert!(!text.contains("Opus"));
        assert!(mini(UsageEntry::default()).ends_with("   usage unknown"));
    }

    #[test]
    fn wrap_keeps_breaks_and_fills_greedily() {
        assert_eq!(wrap("a b c\n\nd", 3), ["a b", "c", "", "d"]);
        assert_eq!(wrap("longword x", 4), ["longword", "x"]);
    }

    #[test]
    fn ellipsize_cuts_with_an_ellipsis() {
        let line = Line::from("abcdefghij");
        assert_eq!(plain_lines(&[ellipsize(line.clone(), 5)]), "abcd…");
        assert_eq!(plain_lines(&[ellipsize(line, 20)]), "abcdefghij");
    }
}
