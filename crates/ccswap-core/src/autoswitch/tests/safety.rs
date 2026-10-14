//! Auto safety: idle-hold, freshening through the consume gate, the token
//! endpoint's identity check, the departure trigger record (upstream
//! `TestIdleHold`, `TestFreshening`, `TestTokenIdentity`,
//! `TestFreshenRoutesThroughGate`,
//! `TestConsumeFirstDepartureRecordsItsOwnTrigger`, `TestReviewFindings202`).

use super::*;
use crate::anthropic_client::TokenAccount;
use crate::autoswitch::rank::{BarInput, left_account_recovered};

fn held() -> Vec<(String, Val)> {
    vec![
        ("1".into(), Val::Sentinel(Sentinel::TokenExpired)),
        ("2".into(), Val::Dict(usage(10.0).unwrap())),
        ("3".into(), Val::Dict(usage(20.0).unwrap())),
    ]
}

fn healthy(active: f64) -> Vec<(String, Val)> {
    vec![
        ("1".into(), Val::Dict(usage(active).unwrap())),
        ("2".into(), Val::Dict(usage(10.0).unwrap())),
        ("3".into(), Val::Dict(usage(20.0).unwrap())),
    ]
}

fn tick_values(h: &Harness, e: &mut AutoSwitchEngine, values: Vec<(String, Val)>) -> TickOutcome {
    h.serve_values(values);
    e.tick()
}

fn switch_trigger(h: &Harness) -> Option<Trigger> {
    h.switch().map(|(t, ..)| t)
}

#[test]
fn idle_hold_test_token_expired_holds_instead_of_failover() {
    let h = Harness::three();
    let mut e = h.default_engine();
    for _ in 0..6 {
        assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::NoAction);
        h.clock.advance(60.0);
    }
    assert_eq!(h.active(), s("1"));
    assert!(!h.has("switch"));
    let reasons: HashSet<&str> = h.reasons().into_iter().collect();
    assert_eq!(reasons, HashSet::from(["active-idle"]));
    assert_eq!(e.unhealthy_ticks, 0);
}

#[test]
fn idle_hold_test_idle_hold_slows_cadence() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let outcome = tick_values(&h, &mut e, held());
    assert_eq!(outcome, TickOutcome::NoAction);
    assert!(e.next_delay(outcome) >= NO_RESET_FALLBACK_S);
}

#[test]
fn idle_hold_test_idle_hold_cap_escalates_to_failover() {
    let h = Harness::three();
    let mut e = h.default_engine();
    assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::NoAction);
    h.clock.advance(IDLE_HOLD_MAX_S + 1.0);
    assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::NoAction);
    assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::NoAction);
    assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::Switched);
    assert_eq!(switch_trigger(&h), Some(Trigger::Failover));
}

#[test]
fn idle_hold_test_recovery_resets_the_hold_clock() {
    let h = Harness::three();
    let mut e = h.default_engine();
    tick_values(&h, &mut e, held());
    h.clock.advance(IDLE_HOLD_MAX_S - 60.0);
    tick_values(&h, &mut e, healthy(50.0));
    h.clock.advance(120.0);
    assert_eq!(tick_values(&h, &mut e, held()), TickOutcome::NoAction);
    assert_eq!(e.unhealthy_ticks, 0);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn idle_hold_test_plain_fetch_failure_still_counts_unhealthy() {
    let h = Harness::three();
    let mut e = h.default_engine();
    tick_values(&h, &mut e, held());
    let unknown = vec![
        ("1".into(), Val::Unknown),
        ("2".into(), Val::Dict(usage(10.0).unwrap())),
        ("3".into(), Val::Dict(usage(20.0).unwrap())),
    ];
    assert_eq!(tick_values(&h, &mut e, unknown), TickOutcome::NoAction);
    assert_eq!(e.unhealthy_ticks, 1);
    assert_eq!(e.idle_hold_since, None);
}

#[test]
fn idle_hold_test_foreign_credential_sentinel_fails_over_instead_of_holding() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let foreign = || {
        vec![
            ("1".into(), Val::Sentinel(Sentinel::ForeignCredential)),
            ("2".into(), Val::Dict(usage(10.0).unwrap())),
            ("3".into(), Val::Dict(usage(20.0).unwrap())),
        ]
    };
    assert_eq!(tick_values(&h, &mut e, foreign()), TickOutcome::NoAction);
    assert_eq!(tick_values(&h, &mut e, foreign()), TickOutcome::NoAction);
    assert_eq!(tick_values(&h, &mut e, foreign()), TickOutcome::Switched);
    assert_eq!(switch_trigger(&h), Some(Trigger::Failover));
    assert_eq!(e.idle_hold_since, None);
}

fn oauth_creds(access: &str, refresh: &str, expires_at: i64) -> String {
    json!({"claudeAiOauth": {"accessToken": access, "refreshToken": refresh,
                             "expiresAt": expires_at}})
    .to_string()
}

fn refreshed(credentials: &str, token_account: Option<TokenAccount>) -> RefreshOutcome {
    RefreshOutcome {
        credentials: Some(credentials.into()),
        token_account,
        ..Default::default()
    }
}

#[test]
fn freshening_test_near_expiry_target_is_refreshed_and_persisted() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host
        .seed_expiring(2, "b@example.com", Some((START * 1000.0) as i64 + 60_000));
    h.host.make_live("a@example.com");
    let rotated = oauth_creds("sk-2-new", "rt-2-new", (START * 1000.0) as i64 + 3_600_000);
    *h.host.refresh.borrow_mut() = Some(refreshed(&rotated, None));
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.host.refresh_calls.get(), 1);
    assert_eq!(h.host.creds.borrow()["2"], rotated);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn freshening_test_live_session_near_expiry_is_skipped() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.make_live("a@example.com");
    h.host.live_sessions.borrow_mut().insert("2".into());
    let mut e = h.default_engine();
    let out = h.tick(&mut e, &[("1", usage(95.0)), ("2", usage(10.0))]);
    assert_eq!(out, TickOutcome::Blocked);
    assert_eq!(h.host.refresh_calls.get(), 0);
    assert_eq!(h.active(), s("1"));
}

const FAR: i64 = 99_999_999_999_000;

fn expire_two(h: &Harness) {
    h.host
        .creds
        .borrow_mut()
        .insert("2".into(), oauth_creds("sk-2", "rt-2", 0));
}

fn set_identity(h: &Harness, num: &str, uuid: Option<&str>, org: Option<&str>) {
    let mut ids = h.host.identities.borrow_mut();
    let id = ids.get_mut(num).unwrap();
    if let Some(uuid) = uuid {
        id.uuid = uuid.into();
    }
    if let Some(org) = org {
        id.organization_uuid = org.into();
    }
}

fn account(uuid: &str, email: &str, org: &str) -> Option<TokenAccount> {
    Some(TokenAccount {
        uuid: uuid.into(),
        email: Some(email.into()),
        organization_uuid: Some(org.into()),
    })
}

#[test]
fn token_identity_test_uuid_backfill_from_token_account_on_freshen() {
    let h = Harness::three();
    set_identity(&h, "2", Some(""), None);
    expire_two(&h);
    let fresh = oauth_creds("sk-2f", "rt-2f", FAR);
    *h.host.refresh.borrow_mut() = Some(refreshed(
        &fresh,
        account("uuid-2-real", "b@example.com", ""),
    ));
    let e = h.default_engine();
    assert_eq!(e.freshen_target("2", "b@example.com"), Freshen::Ok);
    assert_eq!(h.host.identities.borrow()["2"].uuid, "uuid-2-real");
}

#[test]
fn token_identity_test_conflicting_token_identity_returns_identity_conflict() {
    let h = Harness::three();
    expire_two(&h);
    let fresh = oauth_creds("sk-2f", "rt-2f", FAR);
    *h.host.refresh.borrow_mut() = Some(refreshed(
        &fresh,
        account("uuid-somebody-else", "z@example.com", ""),
    ));
    let e = h.default_engine();
    assert_eq!(
        e.freshen_target("2", "b@example.com"),
        Freshen::IdentityConflict
    );
    assert_eq!(h.host.creds.borrow()["2"], fresh);
}

#[test]
fn token_identity_test_identity_conflict_quarantines_instead_of_activating() {
    let h = Harness::three();
    expire_two(&h);
    let fresh = oauth_creds("sk-2f", "rt-2f", FAR);
    *h.host.refresh_fn.borrow_mut() = Some(Box::new(move |_, snapshot| {
        if snapshot.contains("\"rt-2\"") {
            refreshed(&fresh, account("uuid-somebody-else", "z@example.com", ""))
        } else {
            refreshed(snapshot, None)
        }
    }));
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(80.0))],
    );
    assert!(h.has("account-quarantined"));
    assert_eq!(
        h.host.state()["quarantine"]["2"]["reason"],
        json!("identity-conflict")
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn token_identity_test_dead_slot_quarantined_even_with_safety_copy_present() {
    let h = Harness::three();
    h.host
        .creds
        .borrow_mut()
        .insert("2".into(), oauth_creds("sk-2-dead", "rt-2-dead", 0));
    *h.host.refresh_fn.borrow_mut() = Some(Box::new(|_, snapshot| {
        if snapshot.contains("rt-2-dead") {
            RefreshOutcome::failed("invalid_grant")
        } else {
            refreshed(snapshot, None)
        }
    }));
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[("1", usage(95.0)), ("2", usage(10.0)), ("3", usage(80.0))],
    );
    assert_eq!(
        h.host.state()["quarantine"]["2"]["reason"],
        json!("invalid_grant")
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn token_identity_test_same_uuid_different_org_is_identity_conflict() {
    let h = Harness::three();
    set_identity(&h, "2", None, Some("org-2"));
    expire_two(&h);
    *h.host.refresh.borrow_mut() = Some(refreshed(
        &oauth_creds("sk-2f", "rt-2f", FAR),
        account("uuid-2", "b@example.com", "org-other"),
    ));
    let e = h.default_engine();
    assert_eq!(
        e.freshen_target("2", "b@example.com"),
        Freshen::IdentityConflict
    );
}

#[test]
fn token_identity_test_malformed_token_identity_never_breaks_freshen() {
    let h = Harness::three();
    expire_two(&h);
    let fresh = oauth_creds("sk-2f", "rt-2f", FAR);
    let blank = Some(TokenAccount {
        uuid: "  ".into(),
        email: None,
        organization_uuid: None,
    });
    *h.host.refresh.borrow_mut() = Some(refreshed(&fresh, blank));
    let e = h.default_engine();
    assert_eq!(e.freshen_target("2", "b@example.com"), Freshen::Ok);
    assert_eq!(h.host.creds.borrow()["2"], fresh);
    assert!(h.host.backfills.borrow().is_empty());
}

#[test]
fn token_identity_test_blank_uuid_slot_with_org_conflict_quarantines_not_backfills() {
    let h = Harness::three();
    set_identity(&h, "2", Some(""), Some("org-A"));
    expire_two(&h);
    let fresh = oauth_creds("sk-2f", "rt-2f", FAR);
    *h.host.refresh.borrow_mut() = Some(refreshed(
        &fresh,
        account("uuid-real", "z@example.com", "org-B"),
    ));
    let e = h.default_engine();
    assert_eq!(
        e.freshen_target("2", "b@example.com"),
        Freshen::IdentityConflict
    );
    assert_eq!(h.host.identities.borrow()["2"].uuid, "");
    assert!(h.host.backfills.borrow().is_empty());
    assert_eq!(h.host.creds.borrow()["2"], fresh);
}

fn gate_verdict(error: &str) -> Freshen {
    let h = Harness::bare();
    h.host.seed_expiring(2, "b@example.com", Some(1));
    *h.host.refresh.borrow_mut() = Some(RefreshOutcome::failed(error));
    let e = h.default_engine();
    e.freshen_target("2", "b@example.com")
}

#[test]
fn freshen_routes_through_gate_test_lock_contention_is_not_reported_as_network_trouble() {
    assert_eq!(
        gate_verdict("consume-busy"),
        Freshen::Systemic("consume-busy")
    );
}

#[test]
fn freshen_routes_through_gate_test_invalid_client_is_not_reported_as_network_trouble() {
    assert_eq!(
        gate_verdict("invalid_client"),
        Freshen::Systemic("invalid_client")
    );
}

#[test]
fn freshen_routes_through_gate_test_unreadable_stash_is_not_reported_as_network_trouble() {
    assert_eq!(
        gate_verdict("stash-unreadable"),
        Freshen::Systemic("stash-unreadable")
    );
}

#[test]
fn freshen_routes_through_gate_test_store_unmirrored_keeps_its_own_kind() {
    assert_eq!(
        gate_verdict("store-unmirrored"),
        Freshen::Systemic("store-unmirrored")
    );
}

#[test]
fn freshen_routes_through_gate_test_a_real_transient_still_reads_transient() {
    assert_eq!(gate_verdict("transient"), Freshen::Transient);
}

#[test]
fn freshen_routes_through_gate_test_an_actionable_cause_is_not_hidden_by_a_self_clearing_one() {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed_expiring(2, "b@example.com", Some(1));
    h.host.seed_expiring(3, "c@example.com", Some(1));
    h.host.make_live("a@example.com");
    *h.host.refresh_fn.borrow_mut() = Some(Box::new(|num, _| {
        RefreshOutcome::failed(if num == "2" {
            "store-unmirrored"
        } else {
            "consume-busy"
        })
    }));
    let mut e = h.default_engine();
    h.tick(
        &mut e,
        &[
            ("1", usage7(95.0, 95.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
            ("3", usage7(10.0, 10.0, Some(R_LATEST))),
        ],
    );
    let message = h
        .events
        .borrow()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .next_back()
        .expect("an error event");
    assert!(
        message.contains("CLAUDE_SECURESTORAGE_CONFIG_DIR"),
        "{message}"
    );
}

#[test]
fn freshen_routes_through_gate_test_freshen_calls_consume_gate() {
    let h = Harness::bare();
    h.host.seed_expiring(2, "b@example.com", Some(1));
    let snapshot = h.host.creds.borrow()["2"].clone();
    let calls: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
    let seen = calls.clone();
    let fresh = oauth_creds("sk-y", "rt-y", 9_999_999_999_000);
    *h.host.refresh_fn.borrow_mut() = Some(Box::new(move |num, snap| {
        seen.borrow_mut().push((num.into(), snap.into()));
        refreshed(&fresh, None)
    }));
    let e = h.default_engine();
    assert_eq!(e.freshen_target("2", "b@example.com"), Freshen::Ok);
    assert_eq!(*calls.borrow(), [("2".to_string(), snapshot)]);
}

fn recovered_after(state: Value) -> bool {
    let Value::Object(state) = state else {
        unreachable!()
    };
    let decisions = Decisions::new(
        vec![
            ("1".into(), UsageValue::Usage(usage(89.0).unwrap())),
            ("2".into(), UsageValue::Usage(usage(95.0).unwrap())),
        ],
        &[],
    );
    left_account_recovered(&BarInput {
        state: &state,
        decisions: &decisions,
        active_headroom: Some(5.0),
        threshold: AutoSwitchSettings::default().threshold,
        models: &[],
        now: START,
        current: "2",
    })
}

#[test]
fn consume_first_departure_test_the_null_snapshot_answers_differently_by_recorded_trigger() {
    let failover = recovered_after(json!({
        "lastSwitchFrom": "1", "leftHeadroom": null, "leftRecoveryAt": null,
        "leftTrigger": "failover",
    }));
    let ordinary = recovered_after(json!({
        "lastSwitchFrom": "1", "leftHeadroom": null, "leftRecoveryAt": null,
        "leftTrigger": "consume-first",
    }));
    assert!(failover);
    assert!(!ordinary);
}

#[test]
fn consume_first_departure_test_pre_upgrade_null_snapshot_without_left_trigger_still_infers_failover()
 {
    assert!(recovered_after(json!({
        "lastSwitchFrom": "1", "leftHeadroom": null, "leftRecoveryAt": null,
    })));
}

#[test]
fn consume_first_departure_test_a_legacy_record_with_a_real_left_headroom_is_never_forced_through_the_failover_legs()
 {
    assert!(!recovered_after(json!({
        "lastSwitchFrom": "1", "leftHeadroom": 10.0, "leftRecoveryAt": null,
    })));
}

#[test]
fn consume_first_departure_test_end_to_end_consume_first_departure_records_its_own_trigger() {
    let h = Harness::two();
    let mut e = consume_first(&h);
    let out = h.tick(
        &mut e,
        &[
            ("1", usage7(20.0, 20.0, Some(R_LATER))),
            ("2", usage7(10.0, 10.0, Some(R_SOON))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.host.state()["leftTrigger"], json!("consume-first"));
}

#[test]
fn consume_first_departure_test_end_to_end_failover_departure_records_its_own_trigger() {
    let h = Harness::two();
    let mut e = h.default_engine();
    let mut out = TickOutcome::Error;
    for _ in 0..3 {
        out = h.tick(&mut e, &[("1", None), ("2", usage(4.0))]);
        h.clock.advance(60.0);
    }
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.host.state()["leftTrigger"], json!("failover"));
}

fn at(h: &Harness, seconds: f64) -> String {
    iso_utc(h.clock.now() + seconds)
}

#[test]
fn review_findings_202_test_a_short_interval_is_never_lengthened() {
    let h = Harness::three();
    let next_poll_at = h.clock.now() + 5.0;
    h.serve_entries(vec![(
        "1",
        UsageEntry {
            next_poll_at: Some(next_poll_at),
            ..Default::default()
        },
    )]);
    let mut e = h.engine(with(|s| s.interval_seconds = 15.0));
    let delay = e.next_delay(TickOutcome::NoAction);
    assert!(delay <= 15.0 * 1.1, "a 15s interval slept {delay:.1}s");
}

#[test]
fn review_findings_202_test_recovery_reads_the_binding_windows_reset() {
    let h = Harness::three();
    let usage = json!({
        "five_hour": {"pct": 40.0, "resets_at": at(&h, 3600.0)},
        "seven_day": {"pct": 95.0},
    });
    assert_eq!(
        binding_recovery_ts(Some(&usage), &[], h.clock.now()),
        f64::INFINITY
    );
}

#[test]
fn review_findings_202_test_at_limit_still_ranks_by_headroom_when_all_are_above() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let out = h.tick(
        &mut e,
        &[
            ("1", usage_reset(100.0, &at(&h, 60.0))),
            ("2", usage_reset(91.0, &at(&h, 86400.0))),
            ("3", usage_reset(97.0, &at(&h, 120.0))),
        ],
    );
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert_eq!(switch_trigger(&h), Some(Trigger::AtLimit));
}
