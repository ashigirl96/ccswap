//! The all-above-threshold escape, the recovery horizon and the no-return
//! bar's release predicate (upstream `TestEveryAccountAboveThreshold`,
//! `TestRecoveryIsUsefulEitherClause`, `TestRecoveryHorizon`,
//! `TestTheHorizonDoesNotDiscardWhatItAlreadyKnows`,
//! `TestTheReleasePredicateOneStateShapePerTest`,
//! `TestAllSpentGoesToTheSoonestReset`, `TestEscapeBeforeTheLimitLands`).

use super::*;
use crate::autoswitch::rank::{BarInput, RECOVERY_HORIZON_S, recovery_is_useful};

const H: f64 = 3600.0;

fn at(h: &Harness, seconds: f64) -> String {
    iso_utc(h.clock.now() + seconds)
}

fn u(h: &Harness, pct: f64, in_s: f64) -> Option<Value> {
    usage_reset(pct, &at(h, in_s))
}

fn trigger(h: &Harness) -> Trigger {
    h.switch().expect("a switch event").0
}

// --- TestEveryAccountAboveThreshold ------------------------------------------

#[test]
fn every_account_above_threshold_test_moves_to_the_soonest_recovering_account() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 2.0 * H)),
        ("2", u(&h, 100.0, 600.0)),
        ("3", u(&h, 95.0, 480.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn every_account_above_threshold_test_soonest_wins_over_most_headroom() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, H)),
        ("2", u(&h, 91.0, 3.0 * H)),
        ("3", u(&h, 97.0, 300.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn every_account_above_threshold_test_a_single_healthy_peer_still_wins_normally() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, H)),
        ("2", u(&h, 95.0, 60.0)),
        ("3", u(&h, 20.0, 5.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn every_account_above_threshold_test_below_threshold_is_untouched() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [("1", usage(50.0)), ("2", usage(10.0)), ("3", usage(10.0))];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::NoAction);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn every_account_above_threshold_test_all_at_limit_still_reports_exhausted() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 100.0, 600.0)),
        ("2", u(&h, 100.0, 300.0)),
        ("3", u(&h, 100.0, 900.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn every_account_above_threshold_test_unknown_reset_sorts_last_not_first() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, H)),
        ("2", usage(95.0)),
        ("3", u(&h, 97.0, 600.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn every_account_above_threshold_test_does_not_flap_between_two_near_equal_accounts() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 600.0)),
        ("2", u(&h, 98.0, 660.0)),
        ("3", u(&h, 100.0, 600.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Blocked);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn every_account_above_threshold_test_a_meaningfully_sooner_account_still_wins() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, H)),
        ("2", u(&h, 98.0, 600.0)),
        ("3", u(&h, 100.0, 60.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn every_account_above_threshold_test_consume_first_gets_the_same_anti_flap_guard() {
    let h = Harness::three();
    let mut e = consume_first(&h);
    let usage = [
        ("1", u(&h, 99.0, 600.0)),
        ("2", u(&h, 98.0, 660.0)),
        ("3", u(&h, 100.0, 600.0)),
    ];
    assert_eq!(
        h.tick(&mut e, &usage),
        TickOutcome::Blocked,
        "consume-first skipped the recovery hysteresis"
    );
    assert_eq!(h.active(), s("1"));
}

#[test]
fn every_account_above_threshold_test_at_limit_trigger_still_ignores_the_landing_rule() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 100.0, 60.0)),
        ("2", u(&h, 30.0, 86400.0)),
        ("3", u(&h, 95.0, 120.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert_eq!(trigger(&h), Trigger::AtLimit);
}

// --- TestRecoveryIsUsefulEitherClause ----------------------------------------

#[test]
fn recovery_is_useful_either_clause_test_the_active_alone_being_inside_the_horizon_is_enough() {
    let now = 1_000_000.0;
    assert!(recovery_is_useful(
        now + RECOVERY_HORIZON_S + 3600.0,
        now + 1800.0,
        50.0,
        50.0,
        now
    ));
}

#[test]
fn recovery_is_useful_either_clause_test_the_control_neither_inside_falls_back_to_headroom() {
    let now = 1_000_000.0;
    assert!(!recovery_is_useful(
        now + RECOVERY_HORIZON_S + 3600.0,
        now + RECOVERY_HORIZON_S + 7200.0,
        50.0,
        50.0,
        now
    ));
}

// --- TestRecoveryHorizon -------------------------------------------------------

#[test]
fn recovery_horizon_test_a_minutes_away_reset_still_wins() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 91.0, 7200.0)),
        ("2", u(&h, 94.0, 1800.0)),
        ("3", u(&h, 98.0, 480.0)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn recovery_horizon_test_a_days_away_reset_does_not_buy_headroom() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 91.0, 109.0 * H)),
        ("2", u(&h, 94.0, 80.0 * H)),
        ("3", u(&h, 98.0, 50.0 * H)),
    ];
    h.tick(&mut e, &usage);
    assert_eq!(
        h.active(),
        s("1"),
        "traded 9 points of headroom for 2 on a reset nobody reaches today"
    );
}

fn four() -> Harness {
    let h = Harness::bare();
    h.host.seed(1, "a@example.com");
    h.host.seed(2, "b@example.com");
    h.host.seed(3, "c@example.com");
    h.host.seed(4, "d@example.com");
    h.host.make_live("a@example.com");
    h
}

#[test]
fn recovery_horizon_test_an_unreadable_peer_does_not_veto_the_spent_check() {
    let h = four();
    let mut e = h.default_engine();
    let usage = |pct: f64, in_s: f64| Val::Dict(u(&h, pct, in_s).unwrap());
    h.serve_values(vec![
        ("1".into(), usage(99.0, 109.0 * H)),
        ("2".into(), usage(99.0, 80.0 * H)),
        ("3".into(), usage(99.0, 50.0 * H)),
        ("4".into(), Val::Sentinel(Sentinel::TokenExpired)),
    ]);
    assert_eq!(e.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn recovery_horizon_test_an_unreadable_peer_does_not_forge_headroom_for_the_spent_check() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = |pct: f64, in_s: f64| Val::Dict(u(&h, pct, in_s).unwrap());
    h.serve_values(vec![
        ("1".into(), usage(97.5, 500.0 * H)),
        ("2".into(), usage(98.0, 490.0 * H)),
        ("3".into(), Val::Sentinel(Sentinel::TokenExpired)),
    ]);
    assert_eq!(e.tick(), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn recovery_horizon_test_a_weekly_bound_active_does_not_refuse_a_peer_back_in_minutes() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let weekly = at(&h, 109.0 * H);
    let usage = [
        ("1", usage7(10.0, 96.0, Some(&weekly))),
        ("2", u(&h, 98.0, 480.0)),
        ("3", u(&h, 99.0, 90.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn recovery_horizon_test_an_unknown_active_reset_keeps_the_headroom() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", usage(91.0)),
        ("2", u(&h, 98.0, 80.0 * H)),
        ("3", u(&h, 99.0, 50.0 * H)),
    ];
    let out = h.tick(&mut e, &usage);
    assert_eq!(h.active(), s("1"));
    assert_ne!(out, TickOutcome::Switched);
}

#[test]
fn recovery_horizon_test_a_peer_with_real_headroom_still_wins_past_the_horizon() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 97.0, 50.0 * H)),
        ("2", u(&h, 91.0, 109.0 * H)),
        ("3", u(&h, 98.0, 60.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

// --- TestTheHorizonDoesNotDiscardWhatItAlreadyKnows -------------------------

#[test]
fn the_horizon_does_not_discard_what_it_already_knows_test_equal_headroom_past_the_horizon_takes_the_sooner_reset()
 {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 96.0, 300.0 * H)),
        ("2", u(&h, 92.0, 500.0 * H)),
        ("3", u(&h, 92.0, 5.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn the_horizon_does_not_discard_what_it_already_knows_test_a_peer_worth_having_is_not_filtered_out_of_the_worth_having_check()
 {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 97.0, 500.0 * H)),
        ("2", u(&h, 94.01, 400.0 * H)),
        ("3", u(&h, 99.90, 200.0 * H)),
    ];
    let out = h.tick(&mut e, &usage);
    assert_ne!(h.active(), s("3"));
    assert!(out != TickOutcome::Switched || h.active() == s("2"));
}

#[test]
fn the_horizon_does_not_discard_what_it_already_knows_test_an_unchoosable_peer_does_not_veto_the_reset_ranking()
 {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 97.0, 200.0 * H)),
        ("2", u(&h, 97.0, 10.0 * H)),
        ("3", u(&h, 96.95, 500.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

// --- TestTheReleasePredicateOneStateShapePerTest ------------------------------

/// Upstream `_left_account_recovered(state, usage, headroom, active, ..)`:
/// only the left account "2" is measured, the active "1" is not.
fn recovered(threshold: f64, state: Value, peer: Option<Value>, active: Option<f64>) -> bool {
    let Value::Object(state) = state else {
        unreachable!()
    };
    let value = peer.map_or(UsageValue::Unknown, UsageValue::Usage);
    let decisions = Decisions::new(vec![("2".into(), value)], &[]);
    rank::left_account_recovered(&BarInput {
        state: &state,
        decisions: &decisions,
        active_headroom: active,
        threshold,
        models: &[],
        now: START,
        current: "1",
    })
}

fn failover_state() -> Value {
    json!({"lastSwitchFrom": "2", "leftHeadroom": null, "leftRecoveryAt": null})
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_failover_peer_readable_dominant_and_changed_releases()
 {
    assert!(recovered(90.0, failover_state(), usage(85.0), Some(2.0)));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_the_failover_floor_moves_with_the_users_threshold()
 {
    assert!(!recovered(60.0, failover_state(), usage(65.0), Some(2.0)));
    assert!(recovered(71.0, failover_state(), usage(65.0), Some(2.0)));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_failover_peer_frozen_active_burning_holds() {
    let h = Harness::two();
    let mut e = h.default_engine();
    let mut out = TickOutcome::NoAction;
    for _ in 0..3 {
        out = h.tick(&mut e, &[("1", None), ("2", u(&h, 4.0, 400.0 * H))]);
        h.clock.advance(60.0);
    }
    assert_eq!(out, TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));

    h.clock.advance(301.0);
    let out = h.tick(
        &mut e,
        &[("1", u(&h, 96.0, 500.0 * H)), ("2", u(&h, 98.2, 400.0 * H))],
    );
    assert_ne!(out, TickOutcome::Switched);
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_failover_peer_still_poor_holds() {
    assert!(!recovered(90.0, failover_state(), usage(95.0), Some(2.0)));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_failover_peer_still_unreadable_holds() {
    assert!(!recovered(90.0, failover_state(), None, Some(2.0)));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_ordinary_departure_weekly_bound_peer_that_recovered_releases()
 {
    let state = json!({
        "lastSwitchFrom": "2",
        "leftHeadroom": 4.0,
        "leftRecoveryAt": START + 3600.0,
    });
    let peer = usage_reset(96.0, &iso_utc(START + 60.0));
    assert!(recovered(90.0, state, peer, Some(8.0)));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_pre_upgrade_record_keys_absent_releases() {
    assert!(recovered(
        90.0,
        json!({"lastSwitchFrom": "2"}),
        usage(96.0),
        Some(2.0)
    ));
}

#[test]
fn the_release_predicate_one_state_shape_per_test_test_at_limit_escape_still_works() {
    let Value::Object(state) = json!({"lastSwitchFrom": "2"}) else {
        unreachable!()
    };
    let decisions = Decisions::new(
        vec![
            ("1".into(), UsageValue::Usage(usage(100.0).unwrap())),
            ("2".into(), UsageValue::Usage(usage(0.0).unwrap())),
        ],
        &[],
    );
    for active in [Some(0.0), None] {
        for recovered in [true, false] {
            let bar = BarInput {
                state: &state,
                decisions: &decisions,
                active_headroom: active,
                threshold: 90.0,
                models: &[],
                now: START,
                current: "1",
            };
            assert_eq!(
                rank::no_return_account(Trigger::AtLimit, recovered, &bar),
                None
            );
        }
    }
}

// --- TestAllSpentGoesToTheSoonestReset --------------------------------------

#[test]
fn all_spent_goes_to_the_soonest_reset_test_all_spent_moves_to_the_soonest_reset() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 109.0 * H)),
        ("2", u(&h, 99.0, 80.0 * H)),
        ("3", u(&h, 99.0, 50.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("3"));
}

#[test]
fn all_spent_goes_to_the_soonest_reset_test_already_on_the_soonest_stays_put() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 50.0 * H)),
        ("2", u(&h, 99.0, 80.0 * H)),
        ("3", u(&h, 99.0, 109.0 * H)),
    ];
    assert_ne!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("1"));
}

#[test]
fn all_spent_goes_to_the_soonest_reset_test_real_headroom_still_beats_a_sooner_reset() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 98.0, 109.0 * H)),
        ("2", u(&h, 90.0, 80.0 * H)),
        ("3", u(&h, 99.0, 50.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn all_spent_goes_to_the_soonest_reset_test_a_spent_fleet_takes_the_soonest_reset_over_the_most_headroom()
 {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.5, 300.0 * H)),
        ("2", u(&h, 99.5, 10.0 * H)),
        ("3", u(&h, 99.0, 500.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
}

#[test]
fn all_spent_goes_to_the_soonest_reset_test_the_flap_guard_survives_in_the_spent_band() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 50.0 * H)),
        ("2", u(&h, 99.0, 50.0 * H - 60.0)),
        ("3", u(&h, 99.0, 80.0 * H)),
    ];
    assert_ne!(h.tick(&mut e, &usage), TickOutcome::Switched);
}

// --- TestEscapeBeforeTheLimitLands ----------------------------------------

#[test]
fn escape_before_the_limit_lands_test_at_99_the_proactive_path_already_escapes() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 109.0 * H)),
        ("2", u(&h, 70.0, 80.0 * H)),
        ("3", u(&h, 100.0, 50.0 * H)),
    ];
    assert_eq!(h.tick(&mut e, &usage), TickOutcome::Switched);
    assert_eq!(h.active(), s("2"));
    assert_eq!(trigger(&h), Trigger::Proactive);
}

#[test]
fn escape_before_the_limit_lands_test_at_99_with_only_spent_peers_it_holds() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 99.0, 109.0 * H)),
        ("2", u(&h, 100.0, 80.0 * H)),
        ("3", u(&h, 100.0, 50.0 * H)),
    ];
    assert_ne!(h.tick(&mut e, &usage), TickOutcome::Switched);
}

#[test]
fn escape_before_the_limit_lands_test_below_the_brink_the_ordinary_rules_still_decide() {
    let h = Harness::three();
    let mut e = h.default_engine();
    let usage = [
        ("1", u(&h, 50.0, 109.0 * H)),
        ("2", u(&h, 45.0, 80.0 * H)),
        ("3", u(&h, 40.0, 50.0 * H)),
    ];
    assert_ne!(h.tick(&mut e, &usage), TickOutcome::Switched);
}
