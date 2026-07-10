use chrono::{DateTime, Duration, TimeZone, Utc};
use poise_core::replay::estimate_settlement_fee;
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
use poise_core::types::{ExchangeRules, Exposure, QuantityKind};
use poise_engine::decision_evaluation::{DecisionEvaluationInput, run_decision_evaluation};
use poise_engine::decision_replay::{
    DecisionOrderObservation, DecisionReplayEffect, DecisionReplayFillModel, DecisionReplayInput,
    DecisionReplayReleaseTrigger, DecisionScenarioAction, DecisionScenarioStep,
};
use poise_engine::execution_gate::ExecutionGateDecision;
use poise_engine::execution_plan::DecisionReason;
use poise_engine::observation::{MarketObservation, PositionObservation};
use poise_engine::ports::{ExecutionQuote, OrderStatus};
use poise_engine::price_gate::{PriceExecutionBlockReason, PriceExecutionGate};
use poise_engine::reconciler::TargetReconcileResult;
use poise_engine::runtime::InventoryDecisionSnapshot;

#[test]
fn existing_public_decision_structs_remain_constructible_outside_engine_crate() {
    let _reconcile = TargetReconcileResult {
        events: vec![],
        policy_target: Exposure(0.0),
        risk_approved_target: Exposure(0.0),
        execution_target: Exposure(0.0),
        reason: None,
        applied_risk_cap: None,
        new_runtime_state: None,
        execution_gate_decision: ExecutionGateDecision::Open,
        target_pursuit_suppressed: false,
        acquisition_frontier: None,
        risk_acquisition: None,
        fresh_acquisition_initialization: None,
    };
    let _snapshot = InventoryDecisionSnapshot {
        observed_at: at(2026, 7, 1, 9, 0),
        policy_target: Exposure(0.0),
        risk_approved_target: Exposure(0.0),
        acquisition_frontier: None,
        execution_target: Exposure(0.0),
        reason: DecisionReason::AtExecutionTarget,
        applied_risk_cap: None,
        price_execution_gate: PriceExecutionGate::Open,
        revision: 1,
        risk_acquisition: None,
    };
}

#[test]
fn evaluation_rejects_invalid_leverage_horizon_and_missing_live_duration() {
    let started_at = at(2026, 7, 1, 9, 0);
    let mut input = evaluation_input(
        vec![quote_step(started_at, 95.0)],
        started_at + Duration::minutes(1),
        DecisionReplayFillModel::NeverFill,
    );
    input.leverage = 0;
    assert!(
        run_decision_evaluation(&input)
            .unwrap_err()
            .to_string()
            .contains("leverage")
    );

    input.leverage = 10;
    input.ended_at = started_at - Duration::seconds(1);
    assert!(
        run_decision_evaluation(&input)
            .unwrap_err()
            .to_string()
            .contains("ended_at")
    );

    input.ended_at = started_at;
    assert!(
        run_decision_evaluation(&input)
            .unwrap_err()
            .to_string()
            .contains("live duration")
    );

    input.replay.steps = vec![DecisionScenarioStep {
        observed_at: started_at,
        action: DecisionScenarioAction::Market(MarketObservation::MarkPrice { mark_price: 95.0 }),
    }];
    input.ended_at = started_at + Duration::minutes(1);
    assert!(
        run_decision_evaluation(&input)
            .unwrap_err()
            .to_string()
            .contains("ExecutionQuote")
    );
}

#[test]
fn evaluation_preserves_submillisecond_live_duration() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![quote_step(started_at, 100.0)],
        started_at + Duration::microseconds(500),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert_close(report.horizon.live_duration_seconds, 0.000_5);
}

#[test]
fn evaluation_rejects_automatic_restart_comparison_with_post_restart_order_ref() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(1),
                action: DecisionScenarioAction::Restart,
            },
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(1),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.0,
                    status: OrderStatus::New,
                }),
            },
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::NeverFill,
    );

    let error = run_decision_evaluation(&input).unwrap_err().to_string();

    assert!(error.contains("Order(order_ref) after Restart"));
}

#[test]
fn advantage_evaluation_separates_matched_price_improvement_from_tracking_loss() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::minutes(1), 93.8),
            quote_step(started_at + Duration::minutes(2), 92.5),
        ],
        started_at + Duration::minutes(3),
        DecisionReplayFillModel::ImmediateFullAtOrderPrice,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert_eq!(report.fill_model, input.replay.fill_model);
    assert_close(report.horizon.live_duration_seconds, 180.0);
    assert!(!has_release(
        &report.delayed_decision_replay.steps[1].effects
    ));
    assert!(has_release_trigger(
        &report.delayed_decision_replay.steps[2].effects,
        DecisionReplayReleaseTrigger::PriceAdvantage,
    ));
    assert_eq!(report.delayed_metrics.release_count, 1);
    assert_close(report.delayed_metrics.max_single_catch_up_units, 1.5);
    assert!(report.delayed_metrics.max_backlog_units > 0.0);
    assert!(report.comparison.additional_delayed_tracking_error_units > 0.0);
    let long = &report.comparison.long_risk_increase_price;
    assert!(long.curve_immediate_total_native_qty > long.delayed_total_native_qty);
    assert!(long.matched_native_qty > 0.0);
    assert!(long.favorable_price_improvement.unwrap() > 0.0);
}

#[test]
fn offline_restart_sensitivity_uses_only_common_live_intervals() {
    let started_at = at(2026, 7, 1, 9, 0);
    let restarted_at = started_at + Duration::days(31);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            DecisionScenarioStep {
                observed_at: restarted_at,
                action: DecisionScenarioAction::Restart,
            },
            quote_step(restarted_at, 95.0),
            quote_step(restarted_at + Duration::minutes(59), 95.0),
            quote_step(restarted_at + Duration::minutes(60), 95.0),
        ],
        restarted_at + Duration::minutes(61),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let sensitivity = report.restart_sensitivity.unwrap();

    assert_close(report.horizon.live_duration_seconds, 61.0 * 60.0);
    assert!(!has_release(
        &report.delayed_decision_replay.steps[2].effects
    ));
    assert!(!has_release(
        &report.delayed_decision_replay.steps[3].effects
    ));
    assert!(has_release_trigger(
        &report.delayed_decision_replay.steps[4].effects,
        DecisionReplayReleaseTrigger::StaleTimeout,
    ));
    assert!(sensitivity.time_weighted_mean_abs_execution_target_delta_units > 0.0);
    assert_close(
        sensitivity.time_weighted_mean_abs_simulated_position_delta_units,
        0.0,
    );
}

#[test]
fn reversal_exposes_tracking_loss_without_claiming_unmatched_price_improvement() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::minutes(1), 100.0),
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::ImmediateFullAtOrderPrice,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert_close(
        report.delayed_decision_replay.final_simulated_exposure.0,
        0.0,
    );
    assert!(
        report.delayed_metrics.execution.turnover_notional
            < report.curve_immediate_metrics.turnover_notional
    );
    assert!(
        report.delayed_metrics.execution.estimated_taker_fee
            < report.curve_immediate_metrics.estimated_taker_fee
    );
    assert!(report.comparison.additional_delayed_tracking_error_units > 0.0);
    assert_eq!(
        report
            .comparison
            .long_risk_increase_price
            .favorable_price_improvement,
        None
    );
}

#[test]
fn stale_release_occurs_at_threshold_and_not_before() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::minutes(59), 95.0),
            quote_step(started_at + Duration::minutes(60), 95.0),
        ],
        started_at + Duration::minutes(61),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert!(!has_release(
        &report.delayed_decision_replay.steps[1].effects
    ));
    assert!(has_release_trigger(
        &report.delayed_decision_replay.steps[2].effects,
        DecisionReplayReleaseTrigger::StaleTimeout,
    ));
    assert_eq!(report.delayed_metrics.release_count, 1);
    assert_close(report.delayed_metrics.max_single_catch_up_units, 1.0);
}

#[test]
fn cross_zero_flattens_before_releasing_new_short_risk() {
    let started_at = at(2026, 7, 1, 9, 0);
    let mut input = evaluation_input(
        vec![
            quote_step(started_at, 105.0),
            quote_step(started_at + Duration::minutes(1), 107.5),
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::ImmediateFullAtOrderPrice,
    );
    input.replay.initial_position.qty = 2.0;

    let report = run_decision_evaluation(&input).unwrap();
    let delayed = &report.delayed_decision_replay;

    assert_close(delayed.steps[0].simulated_position_qty, 0.0);
    assert!(delayed.steps[0].effects.iter().all(|effect| match effect {
        DecisionReplayEffect::FillOrder {
            risk_increase_native_qty,
            ..
        } => risk_increase_native_qty.abs() < f64::EPSILON,
        _ => true,
    }));
    assert!(delayed.steps[1].effects.iter().any(|effect| matches!(
        effect,
        DecisionReplayEffect::FillOrder {
            side: poise_core::types::Side::Sell,
            risk_increase_native_qty,
            ..
        } if *risk_increase_native_qty > 0.0
    )));
    assert!(has_release_trigger(
        &delayed.steps[1].effects,
        DecisionReplayReleaseTrigger::PriceAdvantage,
    ));
    let short_risk_increase = delayed
        .steps
        .iter()
        .flat_map(|step| &step.effects)
        .filter_map(|effect| match effect {
            DecisionReplayEffect::FillOrder {
                side: poise_core::types::Side::Sell,
                risk_increase_native_qty,
                ..
            } => Some(*risk_increase_native_qty),
            _ => None,
        })
        .sum::<f64>();
    assert!(short_risk_increase > 0.0);
    assert_close(
        report
            .delayed_metrics
            .execution
            .short_risk_increase
            .native_qty,
        short_risk_increase,
    );
    let release_units = delayed.steps[1]
        .effects
        .iter()
        .find_map(|effect| match effect {
            DecisionReplayEffect::RiskReleaseApplied { release_units, .. } => Some(*release_units),
            _ => None,
        })
        .unwrap();
    assert_close(release_units, 1.5);
    assert!(release_units > short_risk_increase);
    assert_close(
        report
            .delayed_metrics
            .execution
            .long_risk_increase
            .native_qty,
        0.0,
    );
    let short = &report.comparison.short_risk_increase_price;
    assert!(short.curve_immediate_total_native_qty > short.delayed_total_native_qty);
    assert_close(short.matched_native_qty, short.delayed_total_native_qty);
    assert!(short.favorable_price_improvement.unwrap() > 0.0);
}

#[test]
fn late_fill_crossing_zero_counts_only_post_zero_risk_but_full_turnover() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 105.0),
            quote_step(started_at + Duration::minutes(1), 107.5),
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.25,
                    unrealized_pnl: 0.0,
                }),
            },
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(3),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 1.0,
                    status: OrderStatus::Filled,
                }),
            },
        ],
        started_at + Duration::minutes(4),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let fill = report.delayed_decision_replay.steps[3]
        .effects
        .iter()
        .find_map(|effect| match effect {
            DecisionReplayEffect::FillOrder {
                quantity,
                price,
                trade_notional,
                risk_increase_native_qty,
                position_before_qty,
                position_after_qty,
                ..
            } => Some((
                *quantity,
                *price,
                *trade_notional,
                *risk_increase_native_qty,
                *position_before_qty,
                *position_after_qty,
            )),
            _ => None,
        })
        .unwrap();

    assert_close(fill.4, 0.25);
    assert_close(fill.5, -0.75);
    assert_close(fill.3, 0.75);
    assert_close(
        fill.2,
        input
            .replay
            .exchange_rules
            .notional_from_native_qty(fill.0, fill.1),
    );
}

#[test]
fn restart_discards_frontier_and_reports_authorization_sensitivity() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::minutes(1), 92.5),
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Restart,
            },
            quote_step(started_at + Duration::minutes(2), 92.5),
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let delayed = &report.delayed_decision_replay;

    assert_close(
        delayed.steps[1]
            .committed_decision
            .as_ref()
            .unwrap()
            .execution_target
            .0,
        1.5,
    );
    assert!(delayed.steps[2].committed_decision.is_none());
    assert_close(
        delayed.steps[3]
            .committed_decision
            .as_ref()
            .unwrap()
            .execution_target
            .0,
        0.0,
    );
    let sensitivity = report.restart_sensitivity.unwrap();
    assert_close(
        sensitivity.time_weighted_mean_abs_execution_target_delta_units,
        0.0,
    );
    assert!(sensitivity.max_abs_execution_target_delta_units > 0.0);
}

#[test]
fn restart_sensitivity_reports_position_difference_when_fills_are_enabled() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::minutes(1), 92.5),
            DecisionScenarioStep {
                observed_at: started_at + Duration::minutes(2),
                action: DecisionScenarioAction::Restart,
            },
            quote_step(started_at + Duration::minutes(2), 90.0),
        ],
        started_at + Duration::minutes(3),
        DecisionReplayFillModel::ImmediateFullAtOrderPrice,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let sensitivity = report.restart_sensitivity.unwrap();

    assert!(sensitivity.max_abs_execution_target_delta_units > 0.0);
    assert!(sensitivity.max_abs_simulated_position_delta_units > 0.0);
}

#[test]
fn never_fill_keeps_position_flat_and_separates_initial_from_catch_up_release() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 100.0),
            quote_step(started_at + Duration::minutes(1), 95.0),
            quote_step(started_at + Duration::minutes(3), 92.5),
        ],
        started_at + Duration::minutes(4),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert_close(
        report.delayed_decision_replay.final_simulated_position_qty,
        0.0,
    );
    assert_close(report.delayed_metrics.execution.turnover_notional, 0.0);
    assert_close(report.delayed_metrics.execution.estimated_taker_fee, 0.0);
    assert_close(report.delayed_metrics.max_backlog_units, 3.0);
    assert_close(
        report.delayed_metrics.time_weighted_mean_backlog_units,
        7.0 / 4.0,
    );
    assert_close(
        report.delayed_metrics.max_authorized_but_unfilled_gap_units,
        3.0,
    );
    assert_close(
        report
            .delayed_metrics
            .time_weighted_mean_authorized_but_unfilled_gap_units,
        7.0 / 4.0,
    );
    assert_close(
        report
            .comparison
            .curve_immediate_time_weighted_tracking_error_units,
        0.0,
    );
    assert_close(
        report.comparison.delayed_time_weighted_tracking_error_units,
        3.5,
    );
    assert_close(
        report.comparison.additional_delayed_tracking_error_units,
        3.5,
    );
    assert_eq!(report.delayed_metrics.release_count, 2);
    assert_close(report.delayed_metrics.max_single_catch_up_units, 1.0);
    assert!(has_release_trigger(
        &report.delayed_decision_replay.steps[1].effects,
        DecisionReplayReleaseTrigger::InitialRatio,
    ));
    assert!(has_release_trigger(
        &report.delayed_decision_replay.steps[2].effects,
        DecisionReplayReleaseTrigger::PriceAdvantage,
    ));
}

#[test]
fn max_metrics_include_same_timestamp_and_horizon_end_states() {
    let started_at = at(2026, 7, 1, 9, 0);
    let scenarios = [
        evaluation_input(
            vec![
                quote_step(started_at, 100.0),
                quote_step(started_at + Duration::minutes(1), 95.0),
                quote_step(started_at + Duration::minutes(1), 100.0),
            ],
            started_at + Duration::minutes(2),
            DecisionReplayFillModel::NeverFill,
        ),
        evaluation_input(
            vec![
                quote_step(started_at, 100.0),
                quote_step(started_at + Duration::minutes(1), 95.0),
            ],
            started_at + Duration::minutes(1),
            DecisionReplayFillModel::NeverFill,
        ),
    ];

    for input in scenarios {
        let report = run_decision_evaluation(&input).unwrap();

        assert_close(report.delayed_metrics.max_backlog_units, 2.0);
        assert_close(
            report.delayed_metrics.max_authorized_but_unfilled_gap_units,
            2.0,
        );
        assert_close(report.comparison.delayed_max_tracking_error_units, 4.0);
        assert_close(report.delayed_metrics.time_weighted_mean_backlog_units, 0.0);
        assert_close(
            report
                .delayed_metrics
                .time_weighted_mean_authorized_but_unfilled_gap_units,
            0.0,
        );
        assert_close(
            report.comparison.delayed_time_weighted_tracking_error_units,
            0.0,
        );
    }
}

#[test]
fn pre_live_position_observation_does_not_inflate_delayed_max_notional() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input_with(
        definition(),
        rules(),
        10.0,
        vec![
            DecisionScenarioStep {
                observed_at: started_at,
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                }),
            },
            quote_step(started_at + Duration::minutes(1), 100.0),
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::NeverFill,
        10,
    );

    let report = run_decision_evaluation(&input).unwrap();

    assert_close(report.delayed_metrics.execution.max_abs_notional, 0.0);
    assert_close(report.curve_immediate_metrics.max_abs_notional, 1_000.0);
}

#[test]
fn partial_fill_and_position_observation_make_exchange_and_manager_facts_converge() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            quote_step(started_at, 95.0),
            quote_step(started_at + Duration::seconds(1), 92.5),
            DecisionScenarioStep {
                observed_at: started_at + Duration::seconds(2),
                action: DecisionScenarioAction::Order(DecisionOrderObservation {
                    order_ref: "order-1".to_string(),
                    filled_qty: 0.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            },
            DecisionScenarioStep {
                observed_at: started_at + Duration::seconds(3),
                action: DecisionScenarioAction::Position(PositionObservation {
                    qty: 0.5,
                    unrealized_pnl: 0.0,
                }),
            },
        ],
        started_at + Duration::seconds(4),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let delayed = &report.delayed_decision_replay;

    assert_close(delayed.steps[2].simulated_position_qty, 0.5);
    assert_close(delayed.steps[2].observed_position_qty, 0.0);
    assert_close(delayed.steps[3].simulated_position_qty, 0.5);
    assert_close(delayed.steps[3].observed_position_qty, 0.5);
    assert!(
        delayed.steps[3]
            .committed_decision
            .as_ref()
            .unwrap()
            .revision
            > delayed.steps[2]
                .committed_decision
                .as_ref()
                .unwrap()
                .revision
    );
    assert!(delayed.steps[2].estimated_taker_fee > 0.0);
    assert_close(delayed.steps[3].estimated_taker_fee, 0.0);
}

#[test]
fn price_gate_hysteresis_matches_committed_decision_and_submit_behavior() {
    let started_at = at(2026, 7, 1, 9, 0);
    let input = evaluation_input(
        vec![
            mark_step(started_at, 100.0),
            quote_step(started_at + Duration::minutes(1), 95.0),
            quote_step(started_at + Duration::minutes(2), 92.5),
            quote_step(started_at + Duration::minutes(3), 98.0),
            quote_step(started_at + Duration::minutes(4), 98.6),
        ],
        started_at + Duration::minutes(5),
        DecisionReplayFillModel::NeverFill,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let delayed = &report.delayed_decision_replay;
    let blocked = PriceExecutionGate::ManualRiskReductionOnly {
        reason: PriceExecutionBlockReason::MarkBookDivergence,
    };

    assert_eq!(
        delayed.steps[3]
            .committed_decision
            .as_ref()
            .unwrap()
            .price_execution_gate,
        blocked
    );
    assert_eq!(
        delayed.steps[3].committed_decision.as_ref().unwrap().reason,
        DecisionReason::PriceGateBlocked
    );
    assert!(
        !delayed.steps[1..=3]
            .iter()
            .any(|step| step.effects.iter().any(|effect| matches!(
                effect,
                DecisionReplayEffect::SubmitOrder {
                    reduce_only: false,
                    ..
                }
            )))
    );
    assert_eq!(
        delayed.steps[4]
            .committed_decision
            .as_ref()
            .unwrap()
            .price_execution_gate,
        PriceExecutionGate::Open
    );
    assert_ne!(
        delayed.steps[4].committed_decision.as_ref().unwrap().reason,
        DecisionReason::PriceGateBlocked
    );
    assert_eq!(report.curve_immediate_benchmark.samples.len(), 4);
    for (sample, expected_price) in report
        .curve_immediate_benchmark
        .samples
        .iter()
        .zip([95.0, 92.5, 98.0, 98.6])
    {
        assert_close(sample.price, expected_price);
    }
    assert!(delayed.steps[4].effects.iter().any(|effect| matches!(
        effect,
        DecisionReplayEffect::SubmitOrder {
            reduce_only: false,
            ..
        }
    )));
}

#[test]
fn inverse_contract_uses_contract_notional_for_quantity_fee_and_margin() {
    let started_at = at(2026, 7, 1, 9, 0);
    let definition = inverse_definition();
    let rules = inverse_rules();
    let input = evaluation_input_with(
        definition,
        rules,
        0.0,
        vec![
            quote_step(started_at, 62_500.0),
            quote_step(started_at + Duration::minutes(1), 60_000.0),
        ],
        started_at + Duration::minutes(2),
        DecisionReplayFillModel::ImmediateFullAtOrderPrice,
        3,
    );

    let report = run_decision_evaluation(&input).unwrap();
    let execution = &report.delayed_metrics.execution;
    let (fill_price, trade_notional) = report
        .delayed_decision_replay
        .steps
        .iter()
        .flat_map(|step| &step.effects)
        .find_map(|effect| match effect {
            DecisionReplayEffect::FillOrder {
                price,
                trade_notional,
                ..
            } => Some((*price, *trade_notional)),
            _ => None,
        })
        .unwrap();
    let expected_fee = estimate_settlement_fee(
        &input.replay.exchange_rules,
        trade_notional,
        fill_price,
        input.replay.exchange_rules.taker_fee_rate,
    )
    .unwrap();

    assert_close(execution.long_risk_increase.native_qty, 3.0);
    assert_close(execution.turnover_notional, 300.0);
    assert_close(execution.max_abs_notional, 300.0);
    assert_close(execution.max_margin_requirement, 100.0);
    assert_eq!(execution.fee_asset, "BTC");
    assert_close(execution.estimated_taker_fee, expected_fee);
    let curve_execution = &report.curve_immediate_metrics;
    assert_close(curve_execution.long_risk_increase.native_qty, 12.0);
    assert_close(curve_execution.turnover_notional, 1_200.0);
    assert_close(curve_execution.max_abs_notional, 1_200.0);
    assert_close(curve_execution.max_margin_requirement, 400.0);
}

fn evaluation_input(
    steps: Vec<DecisionScenarioStep>,
    ended_at: DateTime<Utc>,
    fill_model: DecisionReplayFillModel,
) -> DecisionEvaluationInput {
    evaluation_input_with(definition(), rules(), 0.0, steps, ended_at, fill_model, 10)
}

fn evaluation_input_with(
    definition: TrackDefinition,
    exchange_rules: ExchangeRules,
    initial_position_qty: f64,
    steps: Vec<DecisionScenarioStep>,
    ended_at: DateTime<Utc>,
    fill_model: DecisionReplayFillModel,
    leverage: u32,
) -> DecisionEvaluationInput {
    let started_at = steps
        .first()
        .map(|step| step.observed_at)
        .unwrap_or_else(|| at(2026, 7, 1, 9, 0));
    DecisionEvaluationInput {
        replay: DecisionReplayInput {
            definition,
            exchange_rules,
            started_at,
            initial_position: PositionObservation {
                qty: initial_position_qty,
                unrealized_pnl: 0.0,
            },
            steps,
            fill_model,
        },
        leverage,
        ended_at,
    }
}

fn definition() -> TrackDefinition {
    TrackDefinition::try_new(
        TrackId::new("decision-evaluation"),
        Instrument::new(Venue::Binance, "BTCUSDT"),
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        },
        Some(1_600.0),
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        },
        Some(7_200),
    )
    .unwrap()
}

fn rules() -> ExchangeRules {
    ExchangeRules {
        price_tick: 0.1,
        price_precision: Default::default(),
        quantity_kind: QuantityKind::BaseAsset,
        contract_notional: None,
        settlement_asset: "USDT".to_string(),
        quantity_step: 0.1,
        min_qty: 0.0,
        min_notional: 0.0,
        maker_fee_rate: 0.0002,
        taker_fee_rate: 0.001,
    }
}

fn inverse_definition() -> TrackDefinition {
    TrackDefinition::try_new(
        TrackId::new("inverse-decision-evaluation"),
        Instrument::new(Venue::Binance, "BTCUSD_PERP"),
        TrackConfig {
            lower_price: 60_000.0,
            upper_price: 70_000.0,
            long_exposure_units: 4.0,
            short_exposure_units: 4.0,
            notional_per_unit: 300.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        },
        Some(2_400.0),
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        },
        Some(7_200),
    )
    .unwrap()
}

fn inverse_rules() -> ExchangeRules {
    ExchangeRules {
        price_tick: 0.1,
        price_precision: Default::default(),
        quantity_kind: QuantityKind::InverseContract,
        contract_notional: Some(100.0),
        settlement_asset: "BTC".to_string(),
        quantity_step: 1.0,
        min_qty: 1.0,
        min_notional: 100.0,
        maker_fee_rate: 0.0002,
        taker_fee_rate: 0.001,
    }
}

fn quote_step(observed_at: DateTime<Utc>, price: f64) -> DecisionScenarioStep {
    DecisionScenarioStep {
        observed_at,
        action: DecisionScenarioAction::Market(MarketObservation::ExecutionQuote {
            execution_quote: ExecutionQuote {
                best_bid: price - 0.1,
                best_ask: price + 0.1,
            },
        }),
    }
}

fn mark_step(observed_at: DateTime<Utc>, price: f64) -> DecisionScenarioStep {
    DecisionScenarioStep {
        observed_at,
        action: DecisionScenarioAction::Market(MarketObservation::MarkPrice { mark_price: price }),
    }
}

fn has_release(effects: &[DecisionReplayEffect]) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, DecisionReplayEffect::RiskReleaseApplied { .. }))
}

fn has_release_trigger(
    effects: &[DecisionReplayEffect],
    expected: DecisionReplayReleaseTrigger,
) -> bool {
    effects.iter().any(|effect| {
        matches!(
            effect,
            DecisionReplayEffect::RiskReleaseApplied { trigger, .. } if *trigger == expected
        )
    })
}

fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {actual} to be close to {expected}"
    );
}
