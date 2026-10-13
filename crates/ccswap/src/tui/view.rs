//! Drawing: the dashboard, the switch list, modals, footer and toasts
//! (the layout of upstream `cswap.tcss`).

use ccswap_core::usage::now_secs;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::app::{
    AddTokenModal, App, Confirm, OutputModal, Screen, Severity, SwitchScreen, TOKEN_FIELD_COUNT,
};
use super::theme::Palette;
use super::widgets::{account_card, accounts_panel, wrap};

fn fg(color: ratatui::style::Color) -> Style {
    Style::default().fg(color)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let palette = app.palette();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(palette.background)
                .fg(palette.foreground),
        ),
        area,
    );
    if area.height < 2 {
        return;
    }
    let body = Rect {
        height: area.height - 1,
        ..area
    };
    let footer = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    let base = app
        .screens
        .iter()
        .rposition(|s| !s.is_modal())
        .expect("the dashboard is never modal");
    match &app.screens[base] {
        Screen::Switch(s) => draw_switch(frame, app, s, body, &palette),
        _ => draw_dashboard(frame, app, body, &palette),
    }
    draw_footer(frame, &app.screens[base], footer, &palette);
    for screen in &app.screens[base + 1..] {
        match screen {
            Screen::Confirm(c) => draw_confirm(frame, c, area, &palette),
            Screen::AddToken(m) => draw_add_token(frame, m, area, &palette),
            Screen::Output(o) => draw_output(frame, o, area, &palette),
            _ => {}
        }
    }
    draw_notifications(frame, app, body, &palette);
}

fn inset(area: Rect, top: u16, right: u16, bottom: u16, left: u16) -> Rect {
    let width = area.width.saturating_sub(left + right);
    let height = area.height.saturating_sub(top + bottom);
    Rect {
        x: area.x + left.min(area.width),
        y: area.y + top.min(area.height),
        width,
        height,
    }
}

fn take_top(area: &mut Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    let top = Rect { height, ..*area };
    area.y += height;
    area.height -= height;
    top
}

fn draw_dashboard(frame: &mut Frame, app: &App, mut area: Rect, palette: &Palette) {
    let content_width = area.width.saturating_sub(6).saturating_sub(2) as usize;
    let lines = accounts_panel(
        app.snapshot.as_ref(),
        content_width,
        app.threshold_pct,
        true,
        now_secs(),
        palette,
    );
    let panel = take_top(&mut area, lines.len() as u16 + 2);
    frame.render_widget(Paragraph::new(lines), inset(panel, 1, 3, 1, 3));
    let rule = take_top(&mut area, 1);
    frame.render_widget(
        Paragraph::new("─".repeat(rule.width as usize)).style(fg(palette.panel)),
        rule,
    );

    let dashboard = app.dashboard();
    take_top(&mut area, 1);
    let title = take_top(&mut area, 1);
    frame.render_widget(
        Paragraph::new(dashboard.crumb()).style(fg(palette.muted)),
        inset(title, 0, 3, 0, 3),
    );
    let menu = inset(area, 1, 2, 0, 1);
    let rows = menu.height as usize;
    let first = dashboard.index.saturating_sub(rows.saturating_sub(1));
    for (i, entry) in dashboard
        .entries()
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
    {
        let row = Rect {
            y: menu.y + (i - first) as u16,
            height: 1,
            ..menu
        };
        let highlighted = i == dashboard.index;
        let colour = if entry.muted() {
            palette.muted
        } else {
            palette.foreground
        };
        draw_list_row(
            frame,
            row,
            highlighted,
            vec![Line::styled(entry.label.clone(), fg(colour))],
            palette,
        );
    }
}

/// A list item: a thick left border (accent when highlighted) and padding.
fn draw_list_row(
    frame: &mut Frame,
    area: Rect,
    highlighted: bool,
    lines: Vec<Line<'static>>,
    palette: &Palette,
) {
    let bg = if highlighted {
        palette.surface
    } else {
        palette.background
    };
    frame.render_widget(Block::default().style(Style::default().bg(bg)), area);
    let border = if highlighted {
        palette.accent
    } else {
        palette.background
    };
    for dy in 0..area.height {
        frame.render_widget(
            Paragraph::new("▌").style(fg(border).bg(bg)),
            Rect {
                y: area.y + dy,
                width: 1.min(area.width),
                height: 1,
                ..area
            },
        );
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(bg)),
        inset(area, 0, 1, 0, 2),
    );
}

fn draw_switch(
    frame: &mut Frame,
    app: &App,
    screen: &SwitchScreen,
    mut area: Rect,
    palette: &Palette,
) {
    take_top(&mut area, 1);
    let title = take_top(&mut area, 1);
    frame.render_widget(
        Paragraph::new("switch to which account?").style(fg(palette.muted)),
        inset(title, 0, 3, 0, 3),
    );
    let list = inset(area, 1, 2, 0, 1);
    let Some(snap) = &app.snapshot else {
        return;
    };
    let card_width = list.width.saturating_sub(3) as usize;
    let now = now_secs();
    let cards: Vec<Vec<Line<'static>>> = snap
        .accounts
        .iter()
        .map(|acc| account_card(acc, card_width, None, now, palette))
        .collect();
    let heights: Vec<u16> = cards.iter().map(|c| c.len() as u16 + 1).collect();
    let mut offset = 0u16;
    if let Some(index) = screen.index {
        let bottom: u16 = heights.iter().take(index + 1).sum();
        offset = bottom.saturating_sub(list.height);
    }
    let mut y = 0u16;
    for (i, card) in cards.into_iter().enumerate() {
        let height = heights[i] - 1;
        let top = y;
        y += heights[i];
        if top < offset || top - offset >= list.height {
            continue;
        }
        let visible = height.min(list.height - (top - offset));
        draw_list_row(
            frame,
            Rect {
                y: list.y + top - offset,
                height: visible,
                ..list
            },
            screen.index == Some(i),
            card,
            palette,
        );
    }
}

fn draw_footer(frame: &mut Frame, base: &Screen, area: Rect, palette: &Palette) {
    let keys: &[(&str, &str)] = match base {
        Screen::Switch(_) => &[("enter", "Switch"), ("b", "Best pick"), ("esc", "Back")],
        _ => &[("s", "Switch accounts"), ("w", "Watch"), ("q", "Quit")],
    };
    let mut spans = Vec::new();
    for (key, label) in keys.iter().chain(&[("^t", "Theme")]) {
        spans.push(Span::styled(
            format!(" {key} "),
            fg(palette.accent).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!("{label} "), fg(palette.foreground)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(palette.panel)),
        area,
    );
}

/// A centred modal box; returns the inner content area.
fn modal_box(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    content_height: u16,
    palette: &Palette,
) -> Rect {
    let width = width.min(area.width * 9 / 10).max(10.min(area.width));
    let height = (content_height + 4)
        .min(area.height * 8 / 10)
        .max(3.min(area.height));
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fg(palette.panel))
            .style(Style::default().bg(palette.surface).fg(palette.foreground)),
        rect,
    );
    inset(rect, 2, 3, 1, 3)
}

fn button(label: &str, focused: bool, palette: &Palette) -> Span<'static> {
    let style = if focused {
        Style::default()
            .bg(palette.accent)
            .fg(palette.background)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(palette.panel).fg(palette.foreground)
    };
    Span::styled(format!("  {label}  "), style)
}

fn buttons_line(buttons: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = Vec::new();
    for b in buttons {
        spans.push(Span::raw("  "));
        spans.push(b);
    }
    Line::from(spans).right_aligned()
}

fn modal_lines(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    lines: Vec<Line<'static>>,
    palette: &Palette,
) {
    let inner = modal_box(frame, area, width, lines.len() as u16, palette);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn title_line(title: &str, palette: &Palette) -> Line<'static> {
    Line::styled(
        title.to_string(),
        fg(palette.accent).add_modifier(Modifier::BOLD),
    )
}

fn draw_confirm(frame: &mut Frame, c: &Confirm, area: Rect, palette: &Palette) {
    let width = 64u16.min(area.width * 9 / 10);
    let mut lines = vec![title_line(&c.title, palette), Line::default()];
    lines.extend(
        wrap(&c.message, width.saturating_sub(6) as usize)
            .into_iter()
            .map(|l| Line::styled(l, fg(palette.foreground))),
    );
    lines.push(Line::default());
    lines.push(buttons_line(vec![
        button(&c.yes_label, c.focus_yes, palette),
        button("Cancel", !c.focus_yes, palette),
    ]));
    lines.push(Line::default());
    lines.push(Line::styled(
        format!(
            "← → · enter  ·  y {}  ·  n / esc cancel",
            c.yes_label.to_lowercase()
        ),
        fg(palette.muted),
    ));
    modal_lines(frame, area, width, lines, palette);
}

const TOKEN_PLACEHOLDERS: [&str; TOKEN_FIELD_COUNT] = [
    "token (required)",
    "email label (optional)",
    "slot number (optional)",
];

fn draw_add_token(frame: &mut Frame, m: &AddTokenModal, area: Rect, palette: &Palette) {
    let width = 64u16.min(area.width * 9 / 10);
    let mut lines = vec![
        title_line("Add account from token", palette),
        Line::default(),
    ];
    lines.extend(
        wrap(
            "OAuth setup-token (sk-ant-oat…) or managed API key (sk-ant-api…); the type is auto-detected.",
            width.saturating_sub(6) as usize,
        )
        .into_iter()
        .map(|l| Line::styled(l, fg(palette.foreground))),
    );
    lines.push(Line::default());
    let values = [
        "•".repeat(m.token.chars().count()),
        m.email.clone(),
        m.slot.clone(),
    ];
    for (i, value) in values.into_iter().enumerate() {
        let focused = m.focus == i;
        let marker = Span::styled(
            "▌",
            fg(if focused {
                palette.accent
            } else {
                palette.panel
            }),
        );
        let text = if value.is_empty() {
            Span::styled(TOKEN_PLACEHOLDERS[i], fg(palette.muted))
        } else {
            Span::styled(value, fg(palette.foreground))
        };
        let mut spans = vec![marker, Span::raw(" "), text];
        if focused {
            spans.push(Span::styled(" ", Style::default().bg(palette.foreground)));
        }
        lines.push(Line::from(spans).style(Style::default().bg(palette.background)));
        lines.push(Line::default());
    }
    lines.push(Line::styled(m.error.clone(), fg(palette.sev_crit)));
    lines.push(buttons_line(vec![
        button("Add", m.focus == TOKEN_FIELD_COUNT, palette),
        button("Cancel", m.focus == TOKEN_FIELD_COUNT + 1, palette),
    ]));
    lines.push(Line::default());
    lines.push(Line::styled(
        "enter add  ·  tab next field  ·  esc cancel",
        fg(palette.muted),
    ));
    modal_lines(frame, area, width, lines, palette);
}

fn draw_output(frame: &mut Frame, o: &OutputModal, area: Rect, palette: &Palette) {
    let width = 90u16.min(area.width * 9 / 10);
    let text = o.output.trim_end();
    let text = if text.is_empty() { "(no output)" } else { text };
    let body: Vec<String> = wrap(text, width.saturating_sub(8) as usize);
    let shown = (body.len() as u16).min(20);
    let scroll = o.scroll.min(body.len() as u16 - shown);
    let mut lines = vec![title_line(&o.title, palette), Line::default()];
    lines.extend(
        body.into_iter()
            .skip(scroll as usize)
            .take(shown as usize)
            .map(|l| {
                Line::styled(
                    format!(" {l}"),
                    fg(palette.foreground).bg(palette.background),
                )
            }),
    );
    lines.push(Line::default());
    lines.push(buttons_line(vec![button("Close", true, palette)]));
    lines.push(Line::default());
    lines.push(Line::styled("esc close", fg(palette.muted)));
    modal_lines(frame, area, width, lines, palette);
}

fn draw_notifications(frame: &mut Frame, app: &App, area: Rect, palette: &Palette) {
    let width = 50u16.min(area.width);
    let mut bottom = area.y + area.height;
    for n in app.notifications.iter().rev() {
        let colour = match n.severity {
            Severity::Information => palette.accent,
            Severity::Warning => palette.sev_warn,
        };
        let mut lines = Vec::new();
        if let Some(title) = &n.title {
            lines.push(Line::styled(
                title.clone(),
                fg(colour).add_modifier(Modifier::BOLD),
            ));
        }
        lines.extend(
            wrap(&n.message, width.saturating_sub(4) as usize)
                .into_iter()
                .map(|l| Line::styled(l, fg(palette.foreground))),
        );
        let height = lines.len() as u16 + 2;
        if bottom < area.y + height {
            break;
        }
        let rect = Rect {
            x: area.x + area.width - width,
            y: bottom - height,
            width,
            height,
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_type(BorderType::Thick)
                    .border_style(fg(colour))
                    .style(Style::default().bg(palette.panel))
                    .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
            ),
            rect,
        );
        bottom -= height;
    }
}
