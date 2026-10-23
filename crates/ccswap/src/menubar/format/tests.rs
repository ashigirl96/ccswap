//! Ported from upstream tests/test_menubar.py (display helpers, title,
//! usage log, switch history, snapshot adapter, weekly roll-forward).

use super::*;
use ccswap_claude::snapshot::{AccountKind, AccountSnapshot};
use ccswap_core::usage::Sentinel;
use serde_json::json;

const NOW: f64 = 1_000_000.0;

fn iso(delta_s: f64) -> String {
    utc_isoformat(NOW + delta_s)
}

fn usage() -> Value {
    json!({
        "five_hour": {"pct": 42.0},
        "seven_day": {"pct": 18.0},
        "spend": {"pct": 30.0, "used": 3.0, "limit": 10.0},
    })
}

fn measured(v: Value) -> DisplayUsage {
    DisplayUsage::Measured(v)
}

fn summary(v: Value) -> String {
    usage_summary(&measured(v), NOW, None)
}

fn settings(show_account_name: bool, title_pct: &str) -> MenuBarSettings {
    MenuBarSettings {
        show_account_name,
        title_pct: title_pct.into(),
        ..Default::default()
    }
}

fn title(email: &str, usage: Value, s: &MenuBarSettings) -> String {
    format_title(Some(email), &measured(usage), s, NOW, None)
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

fn row(number: &str, alias: &str, disabled: bool) -> MenuAccount {
    MenuAccount {
        number: number.into(),
        email: "loc@papaya.asia".into(),
        is_active: false,
        display: measured(usage()),
        last_good: None,
        alias: alias.into(),
        disabled,
        fetched_at: None,
    }
}

#[test]
fn usage_summary_dict() {
    assert_eq!(summary(usage()), "5h 42% · 7d 18% · $ 30%");
}

#[test]
fn usage_summary_partial_windows() {
    assert_eq!(summary(json!({"five_hour": {"pct": 5.0}})), "5h 5%");
}

#[test]
fn usage_summary_includes_scoped_model_limits() {
    let u = json!({
        "five_hour": {"pct": 82.0},
        "seven_day": {"pct": 12.0},
        "scoped": [{"name": "Fable", "pct": 4.0}],
        "spend": {"pct": 30.0},
    });
    assert_eq!(summary(u), "5h 82% · 7d 12% · Fable 4% · $ 30%");
}

#[test]
fn usage_summary_scoped_over_limit_marker() {
    let u = json!({"scoped": [{"name": "Fable", "pct": 100.0}]});
    assert_eq!(summary(u), "Fable 100% (!)");
}

#[test]
fn usage_summary_scoped_multiple_and_countdown() {
    let u = json!({"scoped": [
        {"name": "Fable", "pct": 4.0, "resets_at": iso(2.0 * 3600.0)},
        {"name": "Opus", "pct": 55.0},
    ]});
    assert_eq!(summary(u), "Fable 4% (2h 0m) · Opus 55%");
}

#[test]
fn usage_summary_string_sentinel_passthrough() {
    let note = DisplayUsage::Note("no credentials".into());
    assert_eq!(usage_summary(&note, NOW, None), "no credentials");
}

#[test]
fn usage_summary_none() {
    assert_eq!(
        usage_summary(&DisplayUsage::Unavailable, NOW, None),
        "usage unavailable"
    );
}

#[test]
fn usage_summary_seven_day_ahead_of_pace_marker() {
    let u = json!({"seven_day": {"pct": 50.0, "resets_at": iso(6.0 * 86400.0)}});
    assert_eq!(
        usage_summary(&measured(u), NOW, Some(NOW)),
        "7d 50% (ahead) (6d 0h)"
    );
}

#[test]
fn usage_summary_five_hour_never_shows_pace_marker() {
    let u = json!({"five_hour": {"pct": 90.0, "resets_at": iso(4.0 * 3600.0)}});
    assert!(!usage_summary(&measured(u), NOW, Some(NOW)).contains("ahead"));
}

#[test]
fn usage_summary_scoped_ahead_of_pace_marker() {
    let u = json!({"scoped": [{"name": "Fable", "pct": 50.0, "resets_at": iso(6.0 * 86400.0)}]});
    assert_eq!(
        usage_summary(&measured(u), NOW, Some(NOW)),
        "Fable 50% (ahead) (6d 0h)"
    );
}

#[test]
fn usage_summary_maxed_scoped_marker_wins_over_pace() {
    let u = json!({"scoped": [{"name": "Fable", "pct": 100.0, "resets_at": iso(6.0 * 86400.0)}]});
    let out = usage_summary(&measured(u), NOW, Some(NOW));
    assert!(out.contains("(!)"));
    assert!(!out.contains("ahead"));
}

#[test]
fn usage_summary_no_pace_marker_without_fetched_at() {
    let u = json!({"seven_day": {"pct": 50.0, "resets_at": iso(6.0 * 86400.0)}});
    assert!(!summary(u).contains("ahead"));
}

#[test]
fn usage_summary_no_pace_marker_on_window_rolled_to_zero() {
    let u = json!({"seven_day": {"pct": 95.0, "resets_at": iso(-3.0 * 86400.0)}});
    let out = usage_summary(&measured(u), NOW, Some(NOW - 4.0 * 86400.0));
    assert!(!out.contains("ahead"));
    assert!(out.contains("7d 0%"));
}

#[test]
fn usage_summary_scoped_no_pace_marker_on_window_rolled_to_zero() {
    let u = json!({"scoped": [{"name": "Fable", "pct": 95.0, "resets_at": iso(-3.0 * 86400.0)}]});
    let out = usage_summary(&measured(u), NOW, Some(NOW - 4.0 * 86400.0));
    assert!(!out.contains("ahead"));
    assert!(out.contains("Fable 0%"));
}

#[test]
fn usage_summary_rounds_half_to_even_like_python() {
    let u = json!({"five_hour": {"pct": 2.5}, "seven_day": {"pct": 0.5}, "spend": {"pct": 99.5}});
    assert_eq!(summary(u), "5h 2% · 7d 0% · $ 100%");
}

#[test]
fn format_account_label_plain() {
    assert_eq!(
        format_account_label(&row("2", "", false), NOW),
        "2  loc@papaya.asia  5h 42% · 7d 18% · $ 30%"
    );
}

#[test]
fn format_account_label_with_alias() {
    assert_eq!(
        format_account_label(&row("2", "dev", false), NOW),
        "2  dev  (loc@papaya.asia)  5h 42% · 7d 18% · $ 30%"
    );
}

#[test]
fn format_account_label_disabled_marker() {
    assert_eq!(
        format_account_label(&row("2", "", true), NOW),
        "2  loc@papaya.asia  (disabled)  5h 42% · 7d 18% · $ 30%"
    );
}

#[test]
fn format_usage_log_full() {
    let u = json!({
        "five_hour": {"pct": 35.0, "clock": "06:59"},
        "seven_day": {"pct": 55.0, "clock": "Jun 29 21:59"},
    });
    assert_eq!(
        format_usage_log("a@x.com", Some(&u)).unwrap(),
        "usage a@x.com: 5h 35% (resets 06:59) · 7d 55% (resets Jun 29 21:59)"
    );
}

#[test]
fn format_usage_log_without_clock() {
    let u = json!({"five_hour": {"pct": 0.0}, "seven_day": {"pct": 12.0}});
    assert_eq!(
        format_usage_log("a@x.com", Some(&u)).unwrap(),
        "usage a@x.com: 5h 0% · 7d 12%"
    );
}

#[test]
fn format_usage_log_partial_window() {
    let u = json!({"seven_day": {"pct": 12.0, "clock": "Jul 3"}});
    assert_eq!(
        format_usage_log("a@x.com", Some(&u)).unwrap(),
        "usage a@x.com: 7d 12% (resets Jul 3)"
    );
}

#[test]
fn format_usage_log_none_when_no_numeric_window() {
    assert_eq!(format_usage_log("a@x.com", None), None);
    assert_eq!(
        format_usage_log("a@x.com", Some(&json!("rate limited"))),
        None
    );
    assert_eq!(
        format_usage_log("a@x.com", Some(&json!({"spend": {"pct": 5.0}}))),
        None
    );
}

#[test]
fn usage_log_key_ignores_clock_tracks_pct() {
    let u1 = json!({"five_hour": {"pct": 35.0, "clock": "06:59"}, "seven_day": {"pct": 55.0}});
    let u2 = json!({"five_hour": {"pct": 35.0, "clock": "07:59"}, "seven_day": {"pct": 55.0}});
    let u3 = json!({"five_hour": {"pct": 36.0}, "seven_day": {"pct": 55.0}});
    assert_eq!(usage_log_key(Some(&u1)), usage_log_key(Some(&u2)));
    assert_ne!(usage_log_key(Some(&u1)), usage_log_key(Some(&u3)));
    assert_eq!(usage_log_key(None), (None, None));
}

#[test]
fn format_title_name_and_5h() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(true, "5h")),
        "⇄ loc · 42%"
    );
}

#[test]
fn format_title_prefers_alias_over_local_part() {
    let s = settings(true, "off");
    assert_eq!(
        format_title(
            Some("loc@papaya.asia"),
            &measured(usage()),
            &s,
            NOW,
            Some("dev")
        ),
        "⇄ dev"
    );
}

#[test]
fn format_title_name_only_when_pct_off() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(true, "off")),
        "⇄ loc"
    );
}

#[test]
fn format_title_5h_only() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(false, "5h")),
        "⇄ 42%"
    );
}

#[test]
fn format_title_7d_only() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(false, "7d")),
        "⇄ 18%"
    );
}

#[test]
fn format_title_both_windows() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(false, "both")),
        "⇄ 42% · 18%"
    );
}

#[test]
fn format_title_both_windows_with_name() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(true, "both")),
        "⇄ loc · 42% · 18%"
    );
}

#[test]
fn format_title_icon_only_when_off() {
    assert_eq!(
        title("loc@papaya.asia", usage(), &settings(false, "off")),
        "⇄"
    );
}

#[test]
fn format_title_scoped_appends_model_limits() {
    let s = MenuBarSettings {
        title_scoped: true,
        ..settings(true, "off")
    };
    let u = with(usage(), "scoped", json!([{"name": "Fable", "pct": 55.0}]));
    assert_eq!(title("loc@papaya.asia", u, &s), "⇄ loc · Fable 55%");
}

#[test]
fn format_title_scoped_after_windows_multiple_models() {
    let s = MenuBarSettings {
        title_scoped: true,
        ..settings(false, "both")
    };
    let u = with(
        usage(),
        "scoped",
        json!([{"name": "Fable", "pct": 55.0}, {"name": "Opus", "pct": 7.0}]),
    );
    assert_eq!(
        title("loc@papaya.asia", u, &s),
        "⇄ 42% · 18% · Fable 55% · Opus 7%"
    );
}

#[test]
fn format_title_scoped_off_by_default() {
    let s = settings(false, "off");
    assert!(!s.title_scoped);
    let u = with(usage(), "scoped", json!([{"name": "Fable", "pct": 55.0}]));
    assert_eq!(title("loc@papaya.asia", u, &s), "⇄");
}

#[test]
fn format_title_icon_only_when_no_active_account() {
    let s = settings(true, "both");
    assert_eq!(
        format_title(None, &DisplayUsage::Unavailable, &s, NOW, None),
        "⇄"
    );
}

#[test]
fn format_title_truncates_long_local_part() {
    let s = settings(true, "off");
    assert_eq!(
        format_title(
            Some("averylonglocalpart@example.com"),
            &DisplayUsage::Unavailable,
            &s,
            NOW,
            None
        ),
        "⇄ averylonglo*"
    );
}

#[test]
fn format_title_both_drops_unavailable_windows() {
    let s = settings(false, "both");
    let note = DisplayUsage::Note("no credentials".into());
    assert_eq!(format_title(Some("loc@x.com"), &note, &s, NOW, None), "⇄");
}

#[test]
fn format_title_both_keeps_available_window() {
    assert_eq!(
        title(
            "loc@x.com",
            json!({"five_hour": {"pct": 9.0}}),
            &settings(false, "both")
        ),
        "⇄ 9%"
    );
}

#[test]
fn format_title_reflects_passed_weekly_reset() {
    let u = json!({"seven_day": {"pct": 95.0, "resets_at": iso(-86400.0)}});
    assert_eq!(title("a@x.com", u, &settings(false, "7d")), "⇄ 0%");
}

#[test]
fn resets_at_ts_orders_and_handles_missing() {
    let early = json!({"resets_at": "2026-06-24T07:00:00+00:00"});
    let late = json!({"resets_at": "2026-06-26T07:00:00+00:00"});
    assert!(resets_at_ts(Some(&early)) < resets_at_ts(Some(&late)));
    assert_eq!(resets_at_ts(Some(&json!({"pct": 5.0}))), f64::INFINITY);
    assert_eq!(
        resets_at_ts(Some(&json!({"resets_at": "garbage"}))),
        f64::INFINITY
    );
    assert_eq!(resets_at_ts(None), f64::INFINITY);
}

#[test]
fn live_countdown_formats_from_resets_at() {
    let at = |d: f64| json!({"resets_at": iso(d)});
    assert_eq!(
        live_countdown(Some(&at(9.0 * 3600.0 + 5.0 * 60.0)), NOW).unwrap(),
        "9h 5m"
    );
    assert_eq!(
        live_countdown(Some(&at(86400.0 + 19.0 * 3600.0)), NOW).unwrap(),
        "1d 19h"
    );
    assert_eq!(live_countdown(Some(&at(34.0 * 60.0)), NOW).unwrap(), "34m");
}

#[test]
fn live_countdown_none_when_passed_or_missing() {
    assert_eq!(
        live_countdown(Some(&json!({"resets_at": iso(-60.0)})), NOW),
        None
    );
    assert_eq!(live_countdown(Some(&json!({"pct": 5.0})), NOW), None);
    assert_eq!(live_countdown(Some(&json!("no credentials")), NOW), None);
}

#[test]
fn usage_summary_live_countdown_from_resets_at() {
    let u = json!({
        "five_hour": {"pct": 42.0, "resets_at": iso(2.0 * 3600.0 + 33.0 * 60.0)},
        "seven_day": {"pct": 18.0, "resets_at": iso(86400.0 + 19.0 * 3600.0)},
        "spend": {"pct": 30.0},
    });
    assert_eq!(summary(u), "5h 42% (2h 33m) · 7d 18% (1d 19h) · $ 30%");
}

#[test]
fn usage_summary_omits_countdown_when_passed_or_missing() {
    let u = json!({"five_hour": {"pct": 53.0, "resets_at": iso(-60.0)}, "seven_day": {"pct": 8.0}});
    assert_eq!(summary(u), "5h 53% · 7d 8%");
}

const SWITCH_LOG: &str = "2026-06-27 00:57:50,178 - INFO - Switched from account 1 to 3\n\
     2026-06-27 02:06:21,302 - INFO - usage a@x.com: 5h 10%\n\
     2026-06-27 02:10:00,000 - INFO - Switched from account 3 to 1\n";

#[test]
fn parse_switch_history_most_recent_first() {
    assert_eq!(
        parse_switch_history(SWITCH_LOG, SWITCH_HISTORY_LIMIT),
        ["3 → 1   2026-06-27 02:10", "1 → 3   2026-06-27 00:57"]
    );
}

#[test]
fn parse_switch_history_respects_limit() {
    let lines: Vec<String> = (1..6)
        .map(|i| format!("2026-06-27 0{i}:00:00,000 - INFO - Switched from account 1 to 2"))
        .collect();
    let out = parse_switch_history(&lines.join("\n"), 2);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], "1 → 2   2026-06-27 05:00");
}

#[test]
fn parse_switch_history_empty_or_no_matches() {
    assert!(parse_switch_history("", SWITCH_HISTORY_LIMIT).is_empty());
    assert!(parse_switch_history("nothing relevant here", SWITCH_HISTORY_LIMIT).is_empty());
}

#[test]
fn parse_switch_history_skips_a_non_matching_prefix_in_the_same_line() {
    let line =
        "2026-06-27 03:00:00,000 - INFO - Switched from account x; Switched from account 4 to 5";
    assert_eq!(
        parse_switch_history(line, SWITCH_HISTORY_LIMIT),
        ["4 → 5   2026-06-27 03:00"]
    );
}

fn entry(
    sentinel: Option<Sentinel>,
    last_good: Option<Value>,
    fetched_at: Option<f64>,
) -> UsageEntry {
    UsageEntry {
        sentinel,
        last_good,
        fetched_at,
        ..Default::default()
    }
}

#[test]
fn account_display_usage_sentinel_note_last_good_or_none() {
    assert_eq!(
        DisplayUsage::from_entry(&entry(Some(Sentinel::ApiKey), None, None)),
        DisplayUsage::Note("API key (no quota)".into())
    );
    let lg = json!({"five_hour": {"pct": 5.0}});
    assert_eq!(
        DisplayUsage::from_entry(&entry(None, Some(lg.clone()), None)),
        measured(lg)
    );
    assert_eq!(
        DisplayUsage::from_entry(&entry(None, None, None)),
        DisplayUsage::Unavailable
    );
}

fn account(
    number: &str,
    email: &str,
    is_active: bool,
    usage: UsageEntry,
    disabled: bool,
) -> AccountSnapshot {
    AccountSnapshot {
        number: number.into(),
        email: email.into(),
        org_name: String::new(),
        org_uuid: String::new(),
        is_active,
        kind: AccountKind::OAuth,
        switchable: true,
        usage,
        alias: String::new(),
        disabled,
    }
}

#[test]
fn adapt_snapshot_shape_and_active_selection() {
    let lg = json!({"five_hour": {"pct": 10.0}, "seven_day": {"pct": 20.0}});
    let snap = AccountsSnapshot {
        active_number: Some("1".into()),
        accounts: vec![
            account(
                "1",
                "a@x.com",
                true,
                entry(None, Some(lg.clone()), Some(123.0)),
                false,
            ),
            account(
                "2",
                "b@x.com",
                false,
                entry(Some(Sentinel::ApiKey), None, None),
                true,
            ),
        ],
        taken_at: NOW,
    };
    let menu = MenuSnapshot::adapt(&snap);
    assert_eq!(menu.active_email.as_deref(), Some("a@x.com"));
    assert_eq!(menu.active_usage, measured(lg.clone()));
    assert_eq!(menu.active_alias.as_deref(), Some(""));
    assert_eq!(
        menu.accounts[0],
        MenuAccount {
            number: "1".into(),
            email: "a@x.com".into(),
            is_active: true,
            display: measured(lg.clone()),
            last_good: Some(lg),
            alias: String::new(),
            disabled: false,
            fetched_at: Some(123.0),
        }
    );
    assert_eq!(
        menu.accounts[1],
        MenuAccount {
            number: "2".into(),
            email: "b@x.com".into(),
            is_active: false,
            display: DisplayUsage::Note("API key (no quota)".into()),
            last_good: None,
            alias: String::new(),
            disabled: true,
            fetched_at: None,
        }
    );
}

#[test]
fn adapt_snapshot_empty() {
    let snap = AccountsSnapshot {
        active_number: None,
        accounts: Vec::new(),
        taken_at: NOW,
    };
    assert_eq!(MenuSnapshot::adapt(&snap), MenuSnapshot::default());
}

#[test]
fn rolled_weekly_window_advances_passed_reset() {
    let w = json!({"pct": 95.0, "resets_at": iso(-3.0 * 86400.0), "countdown": "stale", "clock": "old"});
    let rolled = rolled_weekly_window(Some(&w), NOW).unwrap();
    assert_eq!(rolled["pct"], 0.0);
    assert!((resets_at_ts(Some(&rolled)) - (NOW + 4.0 * 86400.0)).abs() < 1.0);
    assert!(rolled.get("countdown").is_none() && rolled.get("clock").is_none());
}

#[test]
fn rolled_weekly_window_advances_multiple_missed_weeks() {
    let w = json!({"pct": 80.0, "resets_at": iso(-10.0 * 86400.0)});
    let rolled = rolled_weekly_window(Some(&w), NOW).unwrap();
    assert!((resets_at_ts(Some(&rolled)) - (NOW + 4.0 * 86400.0)).abs() < 1.0);
}

#[test]
fn rolled_weekly_window_leaves_future_or_unknown_untouched() {
    let future = json!({"pct": 42.0, "resets_at": iso(2.0 * 86400.0)});
    assert_eq!(rolled_weekly_window(Some(&future), NOW), Some(future));
    let no_reset = json!({"pct": 42.0});
    assert_eq!(rolled_weekly_window(Some(&no_reset), NOW), Some(no_reset));
    assert_eq!(rolled_weekly_window(None, NOW), None);
}

#[test]
fn rolled_resets_at_is_python_utc_isoformat() {
    let w = json!({"pct": 1.0, "resets_at": "1970-01-01T00:00:00+00:00"});
    let rolled = rolled_weekly_window(Some(&w), 10.0).unwrap();
    assert_eq!(rolled["resets_at"], "1970-01-08T00:00:00+00:00");
}

#[test]
fn usage_summary_reflects_passed_weekly_reset() {
    let u = json!({
        "five_hour": {"pct": 10.0},
        "seven_day": {"pct": 95.0, "resets_at": iso(-86400.0)},
    });
    assert_eq!(summary(u), "5h 10% · 7d 0% (6d 0h)");
}

#[test]
fn a_codex_account_needing_a_login_says_so_in_codex_terms() {
    let listing = ccswap_core::codex::CodexListing {
        accounts: vec![ccswap_core::codex::ListedCodexAccount {
            number: 1,
            email: "c@x.com".into(),
            alias: None,
            plan: "plus".into(),
            account_id: "acct-c".into(),
            active: false,
        }],
    };
    let entry = UsageEntry {
        sentinel: Some(Sentinel::ReloginRequired),
        ..Default::default()
    };
    let accounts = codex_accounts(&listing, &HashMap::from([(1, entry)]));
    assert_eq!(
        accounts[0].display,
        DisplayUsage::Note(
            "re-login needed — Codex could not refresh its token; run: ccswap codex login".into()
        )
    );
}

#[test]
fn usage_summary_scoped_reflects_passed_weekly_reset() {
    let u = json!({"scoped": [{"name": "Fable", "pct": 100.0, "resets_at": iso(-86400.0)}]});
    assert_eq!(summary(u), "Fable 0% (6d 0h)");
}
