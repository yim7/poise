use serde::{Deserialize, Serialize};

mod planning;
mod round_policy;
mod rebalance_trigger;
mod recording;
mod recovery;
mod slots;

#[cfg(test)]
pub(crate) use planning::current_submit_hint;
pub(crate) use planning::{DesiredOrder, ExecutorInput, SubmitIntentInput, plan, refresh_state};
pub use planning::{OrderRole, OrderSlot, PendingSubmitHint};
#[cfg(test)]
pub(crate) use round_policy::{
    RoundLifecycleDecision, evaluate_round_policy, round_policy_input_from_state,
};
pub use recording::OrderUpdateAbsorbResult;
#[cfg(test)]
pub(crate) use recording::apply_order_observation;
pub(crate) use recording::{
    SubmitReceiptResolution, apply_order_observation_with_result, clear_all_working_orders,
    clear_working_order_by_order_id, record_submit_failure, record_submit_receipt,
    record_submit_request,
};
pub use recovery::{RecoveryAnomaly, SubmitRecoveryPlan, SubmitRecoveryResolution};
pub(crate) use recovery::{
    RecoveryInput, RecoveryResolution, SubmitRecoveryInput, recover_submit_effect,
    recover_working_orders, submit_requests_match,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Passive,
    Rebalance,
    CatchUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReason {
    GapEnteredPassive,
    GapEscalatedToRebalance,
    GapEscalatedToCatchUp,
}
pub const INVENTORY_CORE_SLOT: &str = "inventory_core";

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use poise_core::events::ReplacementGateReason;
    use poise_core::types::{ExchangeRules, Exposure, Side};

    use super::*;
    use crate::execution_plan::ExecutionAction;
    use crate::observation::OrderObservation;
    use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
    use crate::runtime::{ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder};
    use crate::track::{Instrument, TrackId, Venue};
    use crate::transition::TrackEffect;

    fn test_track_id() -> TrackId {
        TrackId::new("btc-core")
    }

    fn test_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn test_exchange_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_intent_input<'a>(
        track_id: &'a TrackId,
        instrument: &'a Instrument,
        exchange_rules: &'a ExchangeRules,
        base_qty_per_unit: f64,
        min_rebalance_units: f64,
        current_exposure: Exposure,
        target_exposure: Exposure,
        reference_price: f64,
        observed_at: DateTime<Utc>,
    ) -> SubmitIntentInput<'a> {
        SubmitIntentInput {
            track_id,
            instrument,
            exchange_rules,
            base_qty_per_unit,
            min_rebalance_units,
            current_exposure,
            target_exposure,
            reference_price,
            observed_at,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn executor_input<'a>(
        track_id: &'a TrackId,
        instrument: &'a Instrument,
        exchange_rules: &'a ExchangeRules,
        base_qty_per_unit: f64,
        min_rebalance_units: f64,
        current_exposure: Exposure,
        target_exposure: Exposure,
        reference_price: f64,
        executor_state: Option<&'a ExecutorState>,
        observed_at: DateTime<Utc>,
    ) -> ExecutorInput<'a> {
        ExecutorInput::new(
            submit_intent_input(
                track_id,
                instrument,
                exchange_rules,
                base_qty_per_unit,
                min_rebalance_units,
                current_exposure,
                target_exposure,
                reference_price,
                observed_at,
            ),
            executor_state,
        )
    }

    fn test_executor_state(
        mode: ExecutionMode,
        gap_started_at: Option<DateTime<Utc>>,
    ) -> ExecutorState {
        ExecutorState {
            active_round: Some(crate::runtime::ExecutionRound {
                target_exposure: Exposure(4.0),
                mode: mode.clone(),
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            }),
            diagnostics: crate::runtime::ExecutorDiagnostics {
                mode,
                inventory_gap: Exposure(4.0),
                gap_started_at,
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
                max_inventory_gap_abs: Exposure(4.0),
                max_gap_age_ms: 60_000,
            },
        }
    }

    fn sibling_slot() -> ExecutionSlot {
        ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::Working,
            working_order: Some(WorkingOrder {
                order_id: Some("order-2".into()),
                client_order_id: "client-2".into(),
                side: Side::Sell,
                price: 96.0,
                quantity: 12.0,
                status: OrderStatus::PartiallyFilled,
                role: OrderRole::DecreaseInventory,
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn inventory_core_state(
        now: DateTime<Utc>,
        inventory_gap: Exposure,
        round_target_exposure: Exposure,
        side: Side,
        quantity: f64,
        status: OrderStatus,
        role: OrderRole,
    ) -> ExecutorState {
        ExecutorState {
            active_round: Some(crate::runtime::ExecutionRound {
                target_exposure: round_target_exposure,
                mode: ExecutionMode::Passive,
                started_at: now,
            }),
            diagnostics: crate::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap,
                gap_started_at: Some(now),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side,
                    price: 95.0,
                    quantity,
                    status,
                    role,
                }),
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats::new(now),
        }
    }

    #[test]
    fn plans_execution_mode_from_gap_and_age() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let passive = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(1.0),
            99.9,
            None,
            now,
        ));
        assert_eq!(passive.state.diagnostics.mode, ExecutionMode::Passive);
        assert_eq!(
            passive.state.diagnostics.last_execution_reason,
            Some(ExecutionReason::GapEnteredPassive)
        );

        let rebalance = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(3.0),
            99.9,
            Some(&test_executor_state(
                ExecutionMode::Passive,
                Some(now - Duration::seconds(90)),
            )),
            now,
        ));
        assert_eq!(rebalance.state.diagnostics.mode, ExecutionMode::Rebalance);
        assert_eq!(
            rebalance.state.diagnostics.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToRebalance)
        );

        let catch_up = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(6.0),
            99.9,
            Some(&test_executor_state(
                ExecutionMode::Rebalance,
                Some(now - Duration::seconds(240)),
            )),
            now,
        ));
        assert_eq!(catch_up.state.diagnostics.mode, ExecutionMode::CatchUp);
        assert_eq!(
            catch_up.state.diagnostics.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToCatchUp)
        );
        assert_eq!(catch_up.desired_orders.len(), 1);
        assert!(
            catch_up.state.stats.max_inventory_gap_abs.0
                >= catch_up.state.diagnostics.inventory_gap.0.abs()
        );
    }

    #[test]
    fn passive_mode_keeps_current_working_order_under_small_price_drift() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut existing_state = inventory_core_state(
            now,
            Exposure(1.0),
            Exposure(1.0),
            Side::Buy,
            3.75,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );
        existing_state.diagnostics.last_reprice_at = Some(now - Duration::seconds(30));

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(1.0),
            94.9,
            Some(&existing_state),
            now,
        ));

        assert_eq!(plan.state.diagnostics.mode, ExecutionMode::Passive);
        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
    }

    #[test]
    fn rebalance_mode_replaces_stale_working_order_sooner_than_passive() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let mut passive_state = inventory_core_state(
            now,
            Exposure(1.0),
            Exposure(1.0),
            Side::Buy,
            3.75,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );
        passive_state.diagnostics.gap_started_at = Some(now - Duration::seconds(30));
        passive_state.diagnostics.last_reprice_at = Some(now - Duration::seconds(70));

        let passive_plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(1.0),
            94.9,
            Some(&passive_state),
            now,
        ));
        assert_eq!(passive_plan.state.diagnostics.mode, ExecutionMode::Passive);
        assert_eq!(passive_plan.effects, vec![ExecutionAction::NoOp]);

        let mut rebalance_state = passive_state.clone();
        rebalance_state.diagnostics.gap_started_at = Some(now - Duration::seconds(90));

        let rebalance_plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(1.0),
            94.9,
            Some(&rebalance_state),
            now,
        ));

        assert_eq!(rebalance_plan.state.diagnostics.mode, ExecutionMode::Rebalance);
        assert!(matches!(
            rebalance_plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { .. }
            ] if order_id == "order-1"
        ));
    }

    #[test]
    fn catch_up_mode_uses_most_aggressive_limit_replacement_policy() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut rebalance_state = inventory_core_state(
            now,
            Exposure(1.0),
            Exposure(1.0),
            Side::Buy,
            3.75,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );
        rebalance_state.diagnostics.gap_started_at = Some(now - Duration::seconds(90));
        rebalance_state.diagnostics.last_reprice_at = Some(now - Duration::seconds(25));

        let rebalance_plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(1.0),
            94.9,
            Some(&rebalance_state),
            now,
        ));
        assert_eq!(rebalance_plan.state.diagnostics.mode, ExecutionMode::Rebalance);
        assert_eq!(rebalance_plan.effects, vec![ExecutionAction::NoOp]);

        let mut catch_up_state = rebalance_state.clone();
        catch_up_state.diagnostics.gap_started_at = Some(now - Duration::seconds(240));

        let catch_up_plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(1.0),
            94.9,
            Some(&catch_up_state),
            now,
        ));

        assert_eq!(catch_up_plan.state.diagnostics.mode, ExecutionMode::CatchUp);
        assert!(matches!(
            catch_up_plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { .. }
            ] if order_id == "order-1"
        ));
    }

    #[test]
    fn round_policy_starts_execution_when_gap_requires_action() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let decision = evaluate_round_policy(round_policy_input_from_state(
            &Exposure(0.0),
            &Exposure(4.0),
            None,
            0.5,
            now,
        ));

        assert_eq!(decision.lifecycle, RoundLifecycleDecision::StartRound);
    }

    #[test]
    fn round_policy_continues_execution_when_drift_stays_within_tolerance() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let previous_state = test_executor_state(
            ExecutionMode::Passive,
            Some(now - Duration::seconds(90)),
        );

        let decision = evaluate_round_policy(round_policy_input_from_state(
            &Exposure(0.0),
            &Exposure(4.2),
            Some(&previous_state),
            0.5,
            now,
        ));

        assert_eq!(decision.lifecycle, RoundLifecycleDecision::ContinueRound);
    }

    #[test]
    fn planning_and_recovery_consume_the_same_round_decision() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let previous_state = test_executor_state(
            ExecutionMode::Passive,
            Some(now - Duration::seconds(90)),
        );

        let planning_decision = planning::round_decision_for_test(
            executor_input(
                &track_id,
                &instrument,
                &rules,
                3.75,
                0.5,
                Exposure(0.0),
                Exposure(4.2),
                99.9,
                Some(&previous_state),
                now,
            ),
        );
        let recovery_decision = recovery::round_decision_for_test(
            &Exposure(0.0),
            &Exposure(4.2),
            Some(&previous_state),
            0.5,
            now,
        );

        assert_eq!(planning_decision, recovery_decision);
    }

    #[test]
    fn round_policy_input_from_state_is_shared_by_planning_and_recovery() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let previous_state = test_executor_state(
            ExecutionMode::Passive,
            Some(now - Duration::seconds(90)),
        );

        let planning_input = planning::round_policy_input_for_test(
            &Exposure(0.0),
            &Exposure(4.2),
            Some(&previous_state),
            0.5,
            now,
        );
        let recovery_input = recovery::round_policy_input_for_test(
            &Exposure(0.0),
            &Exposure(4.2),
            Some(&previous_state),
            0.5,
            now,
        );

        assert_eq!(planning_input, recovery_input);
    }

    #[test]
    fn planning_starts_active_round_when_execution_first_begins() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            now,
        ));
        let state_json = serde_json::to_value(&plan.state).unwrap();
        let active_round = state_json["active_round"]
            .as_object()
            .expect("planning should start an active round");

        assert_eq!(active_round["target_exposure"], serde_json::json!(4.0));
    }

    #[test]
    fn refresh_state_preserves_active_round_when_only_desired_exposure_changes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let initial_plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            now,
        ));

        let refreshed = refresh_state(
            &initial_plan.state,
            &Exposure(0.0),
            &Exposure(4.2),
            0.5,
            now + Duration::seconds(30),
        );
        let refreshed_json = serde_json::to_value(&refreshed).unwrap();
        let active_round = refreshed_json["active_round"]
            .as_object()
            .expect("refresh_state should keep the existing active round");

        assert_eq!(active_round["target_exposure"], serde_json::json!(4.0));
    }

    #[test]
    fn recovery_uses_active_round_target_when_receipt_and_live_order_are_replayed() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let planned = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            now,
        ));
        let submit_effect = planned
            .effects
            .iter()
            .find_map(|effect| match effect {
                ExecutionAction::SubmitOrder { request, .. } => Some(request.clone()),
                _ => None,
            })
            .expect("planning should emit a submit order");

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            target_exposure: None,
            min_rebalance_units: 0.5,
            previous_state: Some(&planned.state),
            live_orders: &[OrderObservation {
                order_id: "order-1".into(),
                client_order_id: submit_effect.client_order_id.clone(),
                side: submit_effect.side,
                price: submit_effect.price,
                quantity: submit_effect.quantity,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }],
            pending_submit_hints: &[],
            observed_at: now + Duration::seconds(1),
        });
        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("recovery should rebuild the working slot");
        };
        let state_json = serde_json::to_value(&state).unwrap();
        let active_round = state_json["active_round"]
            .as_object()
            .expect("recovery should preserve active round");
        let working_order = state_json["slots"][0]["working_order"]
            .as_object()
            .expect("rebuilt slot should keep working order");

        assert_eq!(active_round["target_exposure"], serde_json::json!(4.0));
        assert!(
            !working_order.contains_key("target_exposure"),
            "working order should not persist its own target copy"
        );
    }

    #[test]
    fn cancel_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut existing_state = test_executor_state(ExecutionMode::Passive, Some(now));
        existing_state.slots.push(sibling_slot());

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(4.0),
            Exposure(4.0),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert!(matches!(
            plan.effects.as_slice(),
            [ExecutionAction::CancelOrder { order_id, .. }] if order_id == "order-1"
        ));
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn empty_slot_below_min_rebalance_units_does_not_start_new_round() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.5,
            Exposure(2.0),
            Exposure(2.4),
            97.0,
            None,
            now,
        ));

        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
        assert_eq!(
            plan.state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }]
        );
    }

    #[test]
    fn working_order_target_drift_within_min_rebalance_units_is_kept() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = inventory_core_state(
            now,
            Exposure(0.8),
            Exposure(2.8),
            Side::Buy,
            0.8,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            1.0,
            0.5,
            Exposure(2.0),
            Exposure(3.1),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
        assert!(plan.desired_orders.is_empty());
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn reduce_only_working_order_is_kept_when_small_target_drift_crosses_zero() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = inventory_core_state(
            now,
            Exposure(-0.8),
            Exposure(0.2),
            Side::Sell,
            0.8,
            OrderStatus::PartiallyFilled,
            OrderRole::DecreaseInventory,
        );

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            1.0,
            0.5,
            Exposure(1.0),
            Exposure(-0.1),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
        assert!(plan.desired_orders.is_empty());
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn working_order_is_replanned_when_current_exposure_crosses_anchor_direction_even_if_target_drift_is_within_threshold()
     {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = inventory_core_state(
            now,
            Exposure(-1.0),
            Exposure(4.0),
            Side::Buy,
            1.0,
            OrderStatus::PartiallyFilled,
            OrderRole::IncreaseInventory,
        );

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            1.0,
            0.5,
            Exposure(5.0),
            Exposure(3.8),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert!(matches!(
            plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder {
                    request,
                    target_exposure,
                }
            ] if order_id == "order-1"
                && request.side == Side::Sell
                && request.reduce_only
                && (request.quantity - 1.2).abs() < 1e-9
                && *target_exposure == Exposure(3.8)
        ));
    }

    #[test]
    fn submit_pending_target_drift_within_min_rebalance_units_is_kept() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 95.0,
            quantity: 0.8,
            client_order_id: "client-pending".into(),
            reduce_only: false,
        };
        let mut seeded_state = ExecutorState::empty(now);
        seeded_state.active_round = Some(crate::runtime::ExecutionRound {
            target_exposure: Exposure(2.8),
            mode: ExecutionMode::Passive,
            started_at: now,
        });
        let existing_state = record_submit_request(&seeded_state, &request, Exposure(2.8));

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            1.0,
            0.5,
            Exposure(2.0),
            Exposure(3.1),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert_eq!(plan.effects, vec![ExecutionAction::NoOp]);
        assert!(plan.desired_orders.is_empty());
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn working_order_target_drift_crossing_min_rebalance_units_replans() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = inventory_core_state(
            now,
            Exposure(0.8),
            Exposure(2.8),
            Side::Buy,
            0.8,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            1.0,
            0.5,
            Exposure(2.0),
            Exposure(3.4),
            95.0,
            Some(&existing_state),
            now,
        ));

        assert!(matches!(
            plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { target_exposure, .. }
            ] if order_id == "order-1" && *target_exposure == Exposure(3.4)
        ));
    }

    #[test]
    fn target_gap_within_float_tolerance_of_min_rebalance_units_is_not_suppressed() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let min_rebalance_units: f64 = 0.1;
        let near_equal_gap = f64::from_bits(min_rebalance_units.to_bits() - 1);

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            10.0,
            min_rebalance_units,
            Exposure(0.0),
            Exposure(near_equal_gap),
            95.0,
            None,
            now,
        ));

        assert!(matches!(
            plan.effects.as_slice(),
            [ExecutionAction::SubmitOrder {
                request,
                target_exposure,
            }] if request.quantity > 0.0
                && *target_exposure == Exposure(near_equal_gap)
        ));
    }

    #[test]
    fn plan_uses_rounded_order_values_when_checking_exchange_floor() {
        let instrument = test_instrument();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let min_qty_rules = ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.3,
            min_qty: 0.5,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        let min_qty_plan = plan(executor_input(
            &track_id,
            &instrument,
            &min_qty_rules,
            1.0,
            0.0,
            Exposure(0.0),
            Exposure(0.55),
            97.0,
            None,
            now,
        ));
        assert_eq!(min_qty_plan.effects, vec![ExecutionAction::NoOp]);

        let min_notional_rules = ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 10.5,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        let min_notional_plan = plan(executor_input(
            &track_id,
            &instrument,
            &min_notional_rules,
            1.0,
            0.0,
            Exposure(0.0),
            Exposure(10.0),
            1.09,
            None,
            now,
        ));
        assert_eq!(min_notional_plan.effects, vec![ExecutionAction::NoOp]);
    }

    #[test]
    fn replace_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            90.0,
            Some(&existing_state),
            now,
        ));

        assert!(matches!(
            plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { .. }
            ] if order_id == "order-1"
        ));
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn current_submit_hint_returns_single_submit_effect_from_plan() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let hint = current_submit_hint(submit_intent_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            now,
        ));

        let hint = hint.expect("expected current plan to expose a single submit hint");
        assert_eq!(hint.request.instrument, instrument);
        assert_eq!(
            hint.request.client_order_id,
            format!("btc-core-{}", now.timestamp_millis())
        );
        assert!(!hint.request.reduce_only);
        assert_eq!(hint.request.side, Side::Buy);
        assert_eq!(hint.request.price, 95.0);
        assert_eq!(hint.request.quantity, 15.0);
        assert_eq!(hint.target_exposure, Exposure(4.0));
    }

    #[test]
    fn current_submit_hint_returns_none_when_current_intent_needs_no_submit() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let hint = current_submit_hint(submit_intent_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(2.0),
            Exposure(2.0),
            90.0,
            now,
        ));

        assert!(hint.is_none());
    }

    #[test]
    fn plan_sets_reduce_only_for_decrease_inventory_order() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(6.0),
            Exposure(2.0),
            95.0,
            None,
            now,
        ));

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert!(submit.unwrap().reduce_only);
    }

    #[test]
    fn plan_does_not_set_reduce_only_for_increase_inventory_order() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            now,
        ));

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert!(!submit.unwrap().reduce_only);
    }

    #[test]
    fn plan_does_not_set_reduce_only_when_increasing_short_inventory() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(-0.5),
            Exposure(-2.0),
            95.0,
            None,
            now,
        ));

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert_eq!(submit.unwrap().side, Side::Sell);
        assert!(!submit.unwrap().reduce_only);
        assert_eq!(
            plan.desired_orders.first().map(|order| order.role.clone()),
            Some(OrderRole::IncreaseInventory)
        );
    }

    #[test]
    fn plan_sets_reduce_only_when_buying_to_reduce_short_inventory() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(-2.0),
            Exposure(-0.5),
            95.0,
            None,
            now,
        ));

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert_eq!(submit.unwrap().side, Side::Buy);
        assert!(submit.unwrap().reduce_only);
        assert_eq!(
            plan.desired_orders.first().map(|order| order.role.clone()),
            Some(OrderRole::DecreaseInventory)
        );
    }

    #[test]
    fn plan_does_not_set_reduce_only_when_crossing_from_short_to_long() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(-2.0),
            Exposure(1.0),
            95.0,
            None,
            now,
        ));

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert_eq!(submit.unwrap().side, Side::Buy);
        assert!(!submit.unwrap().reduce_only);
        assert_eq!(
            plan.desired_orders.first().map(|order| order.role.clone()),
            Some(OrderRole::IncreaseInventory)
        );
    }

    #[test]
    fn record_submit_request_uses_reduce_only_flag_instead_of_side() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let state = ExecutorState::empty(now);
        let non_reduce_only_sell = OrderRequest {
            instrument: test_instrument(),
            side: Side::Sell,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-sell".into(),
            reduce_only: false,
        };
        let reduce_only_buy = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-buy".into(),
            reduce_only: true,
        };

        let non_reduce_state = record_submit_request(&state, &non_reduce_only_sell, Exposure(-4.0));
        let reduce_state = record_submit_request(&state, &reduce_only_buy, Exposure(-1.0));

        assert_eq!(
            non_reduce_state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.role.clone()),
            Some(OrderRole::IncreaseInventory)
        );
        assert_eq!(
            reduce_state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.role.clone()),
            Some(OrderRole::DecreaseInventory)
        );
    }

    #[test]
    fn replacement_gate_threshold_uses_exchange_maker_and_taker_fee_rate() {
        let instrument = test_instrument();
        let track_id = test_track_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let low_fee_rules = test_exchange_rules();
        let low_fee_plan = plan(executor_input(
            &track_id,
            &instrument,
            &low_fee_rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            94.9,
            Some(&existing_state),
            now,
        ));

        let mut high_fee_rules = test_exchange_rules();
        high_fee_rules.maker_fee_rate = 0.0005;
        high_fee_rules.taker_fee_rate = 0.001;
        let high_fee_plan = plan(executor_input(
            &track_id,
            &instrument,
            &high_fee_rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            94.9,
            Some(&existing_state),
            now,
        ));

        assert_eq!(low_fee_plan.replacement_gate_reason, None);
        assert!(matches!(
            low_fee_plan.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { .. }
            ] if order_id == "order-1"
        ));
        assert!(matches!(
            high_fee_plan.replacement_gate_reason,
            Some(ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 10.5,
                threshold_bps: 20.0,
            })
        ));
        assert_eq!(high_fee_plan.effects, vec![ExecutionAction::NoOp]);
    }

    #[test]
    fn submit_receipt_promotes_submit_pending_slot_to_working() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };

        let pending = record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));
        assert_eq!(pending.slots.len(), 1);
        assert_eq!(pending.slots[0].slot, OrderSlot::new("inventory_core"));
        assert_eq!(pending.slots[0].state, SlotState::SubmitPending);
        assert_eq!(
            pending.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            None
        );

        let SubmitReceiptResolution::Recorded { state: working } = record_submit_receipt(
            &pending,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        ) else {
            panic!("expected matching submit receipt to promote slot");
        };
        assert_eq!(working.slots.len(), 1);
        assert_eq!(working.slots[0].state, SlotState::Working);
        assert_eq!(
            working.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-1")
        );
        assert_eq!(
            working.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::New)
        );
    }

    #[test]
    fn submit_receipt_without_matching_slot_is_rejected() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let mut state = ExecutorState::empty(now);
        state.slots.push(sibling_slot());

        let resolution = record_submit_receipt(
            &state,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_receipt_requires_matching_order_id_once_slot_is_receipt_backed() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: receipt_backed,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let resolution = record_submit_receipt(
            &receipt_backed,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-2".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_receipt_is_rejected_when_multiple_slots_match_same_client_order_id() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let mut state = record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));
        state.slots.push(ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::SubmitPending,
            working_order: Some(WorkingOrder {
                order_id: None,
                client_order_id: "client-1".into(),
                side: Side::Sell,
                price: 96.0,
                quantity: 12.0,
                status: OrderStatus::Submitting,
                role: OrderRole::DecreaseInventory,
            }),
        });

        let resolution = record_submit_receipt(
            &state,
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        );

        assert!(matches!(resolution, SubmitReceiptResolution::Unmatched));
    }

    #[test]
    fn submit_failure_does_not_clear_receipt_backed_slot_with_same_client_order_id() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: receipt_backed,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let next_state = record_submit_failure(&receipt_backed, &request.client_order_id);

        assert_eq!(next_state, receipt_backed);
    }

    #[test]
    fn terminal_order_clears_matching_slot_to_empty() {
        let instrument = test_instrument();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument,
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded { state: working } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        ) else {
            panic!("expected initial receipt to be recorded");
        };

        let cleared = apply_order_observation(
            &working,
            &OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );
        assert_eq!(
            cleared.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }]
        );

        let unchanged = apply_order_observation(
            &working,
            &OrderObservation {
                order_id: "order-2".into(),
                client_order_id: "client-2".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );
        assert_eq!(unchanged.slots, working.slots);
    }

    #[test]
    fn recovery_marks_unknown_live_order_when_no_slot_can_be_inferred() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            target_exposure: None,
            min_rebalance_units: 0.5,
            previous_state: Some(&ExecutorState::empty(now)),
            live_orders: &[OrderObservation {
                order_id: "live-1".into(),
                client_order_id: "unexpected-live".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }],
            pending_submit_hints: &[],
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }

    #[test]
    fn recovery_marks_unknown_live_order_without_historical_slot_even_when_target_exists() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            min_rebalance_units: 0.5,
            previous_state: Some(&ExecutorState::empty(now)),
            live_orders: &[OrderObservation {
                order_id: "live-1".into(),
                client_order_id: "unexpected-live".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                realized_pnl: 0.0,
                status: OrderStatus::New,
            }],
            pending_submit_hints: &[],
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }

    #[test]
    fn recovery_rebuilds_multiple_live_orders_into_distinct_slots() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut previous_state = test_executor_state(ExecutionMode::Passive, Some(now));
        previous_state.slots.push(sibling_slot());

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            min_rebalance_units: 0.5,
            previous_state: Some(&previous_state),
            live_orders: &[
                OrderObservation {
                    order_id: "order-2".into(),
                    client_order_id: "client-2".into(),
                    side: Side::Sell,
                    price: 96.0,
                    quantity: 12.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                },
                OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::PartiallyFilled,
                },
            ],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected two uniquely matched live orders to be rebuilt");
        };
        assert!(state.diagnostics.recovery_anomaly.is_none());
        assert_eq!(state.slots.len(), 2);
        assert_eq!(state.slots[0].slot, OrderSlot::new("inventory_core"));
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-1")
        );
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::PartiallyFilled)
        );
        assert_eq!(state.slots[1].slot, OrderSlot::new("inventory_followup"));
        assert_eq!(
            state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-2")
        );
        assert_eq!(
            state.slots[1]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::New)
        );
    }

    #[test]
    fn recovery_returns_anomaly_state_when_multiple_live_orders_claim_same_slot() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(0.0),
            target_exposure: Some(&Exposure(4.0)),
            min_rebalance_units: 0.5,
            previous_state: Some(&previous_state),
            live_orders: &[
                OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                },
                OrderObservation {
                    order_id: "live-2".into(),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::PartiallyFilled,
                },
            ],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Anomaly { state, anomaly } = recovery else {
            panic!("expected duplicate live orders on one slot to raise anomaly");
        };
        assert_eq!(anomaly, RecoveryAnomaly::DuplicateLiveOrders);
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
        assert_eq!(
            state.diagnostics.recovery_anomaly.as_ref(),
            Some(&RecoveryAnomaly::DuplicateLiveOrders)
        );
    }

    #[test]
    fn submit_recovery_restores_live_order_from_receipt_backed_slot() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };
        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(4.0),
            current_exposure: &Exposure(0.0),
            live_order: Some(&OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::PartiallyFilled,
            }),
            current_plan: None,
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Recovered { state },
            effects,
        } = recovery
        else {
            panic!("expected receipt-backed live order to be recovered");
        };
        assert!(effects.is_empty());
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].state, SlotState::Working);
        assert_eq!(
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.status),
            Some(OrderStatus::PartiallyFilled)
        );
    }

    #[test]
    fn submit_recovery_supersedes_stale_effect_when_current_plan_changed() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 94.0,
            quantity: 22.5,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(6.0));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(6.0),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                3.75,
                0.0,
                Exposure(0.0),
                Exposure(4.0),
                95.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!("expected stale submit effect to be superseded");
        };
        assert_eq!(
            effects,
            vec![ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument,
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    client_order_id: format!("track-1-{}", now.timestamp_millis()),
                    reduce_only: false,
                },
                target_exposure: Exposure(4.0),
            }]
        );
        assert_eq!(
            state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: Some(WorkingOrder {
                    order_id: None,
                    client_order_id: format!("track-1-{}", now.timestamp_millis()),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    status: OrderStatus::Submitting,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
    }

    #[test]
    fn submit_recovery_does_not_supersede_receipt_backed_working_order_when_plan_changes() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Sell,
            price: 100.0,
            quantity: 16.9,
            client_order_id: "client-large-sell".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(-10.0)),
            &request,
            Exposure(-10.0),
            &OrderReceipt {
                order_id: "order-large-sell".into(),
                client_order_id: "client-large-sell".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected receipt-backed working order");
        };

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(-10.0),
            current_exposure: &Exposure(-9.6),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                0.0169,
                0.0,
                Exposure(-9.6),
                Exposure(-9.2),
                95.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects,
        } = recovery
        else {
            panic!("receipt-backed working order should wait for exchange state");
        };
        assert!(effects.is_empty());
    }

    #[test]
    fn submit_recovery_target_reached_marks_receipt_backed_order_as_recent_terminal() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Sell,
            price: 100.0,
            quantity: 16.9,
            client_order_id: "client-large-sell".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(-10.0)),
            &request,
            Exposure(-10.0),
            &OrderReceipt {
                order_id: "order-large-sell".into(),
                client_order_id: "client-large-sell".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected receipt-backed working order");
        };

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(-10.0),
            current_exposure: &Exposure(-10.0),
            live_order: None,
            current_plan: None,
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Recovered { state },
            effects,
        } = recovery
        else {
            panic!("target reached should recover receipt-backed order without follow-up effects");
        };
        assert!(effects.is_empty());

        let replay = apply_order_observation_with_result(
            &state,
            &OrderObservation {
                order_id: "order-large-sell".into(),
                client_order_id: "client-large-sell".into(),
                side: Side::Sell,
                price: 100.0,
                quantity: 16.9,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        );

        assert_eq!(
            replay.absorb_result,
            OrderUpdateAbsorbResult::DuplicateReplay
        );
        assert_eq!(replay.state, state);
    }

    #[test]
    fn submit_recovery_does_not_overwrite_receipt_backed_large_order_with_current_small_submit() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let previous_state = inventory_core_state(
            now,
            Exposure(0.4),
            Exposure(-10.0),
            Side::Sell,
            16.9,
            OrderStatus::New,
            OrderRole::IncreaseInventory,
        );
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 95.0,
            quantity: 0.8,
            client_order_id: "client-small-buy".into(),
            reduce_only: true,
        };

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(-9.2),
            current_exposure: &Exposure(-10.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.0,
                Exposure(-10.0),
                Exposure(-9.2),
                95.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::AwaitExchangeState,
            effects,
        } = recovery
        else {
            panic!("foreign receipt-backed working order should block small follow-up submit");
        };
        assert!(effects.is_empty());
    }

    #[test]
    fn submit_requests_match_rejects_different_reduce_only_semantics() {
        let rules = test_exchange_rules();
        let left = OrderRequest {
            instrument: test_instrument(),
            side: Side::Sell,
            price: 100.0,
            quantity: 3.8,
            client_order_id: "client-1".into(),
            reduce_only: true,
        };
        let right = OrderRequest {
            instrument: test_instrument(),
            side: Side::Sell,
            price: 100.0,
            quantity: 3.8,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };

        assert!(!submit_requests_match(&left, &right, &rules));
    }

    #[test]
    fn submit_requests_match_ignores_client_order_id() {
        let rules = test_exchange_rules();
        let left = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 3.8,
            client_order_id: "btc-core-1711699500000".into(),
            reduce_only: false,
        };
        let right = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 3.8,
            client_order_id: "btc-core-1711699500050".into(),
            reduce_only: false,
        };

        assert!(submit_requests_match(&left, &right, &rules));
    }

    #[test]
    fn plan_generates_unique_client_order_ids_across_calls() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let track_id = test_track_id();
        let t1 = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let t2 = t1 + Duration::milliseconds(1);

        let plan1 = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            t1,
        ));
        let plan2 = plan(executor_input(
            &track_id,
            &instrument,
            &rules,
            3.75,
            0.0,
            Exposure(0.0),
            Exposure(4.0),
            95.0,
            None,
            t2,
        ));

        let id1 = plan1.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request.client_order_id.clone()),
            _ => None,
        });
        let id2 = plan2.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request.client_order_id.clone()),
            _ => None,
        });

        assert!(id1.is_some());
        assert!(id2.is_some());
        assert_ne!(id1, id2);
        assert!(id1.unwrap().starts_with("btc-core-"));
    }

    #[test]
    fn submit_recovery_proceed_updates_slot_target_to_current_plan_target() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 4.0,
            client_order_id: "track-1-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(6.0));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(6.0),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.0,
                Exposure(0.0),
                Exposure(4.0),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution:
                SubmitRecoveryResolution::Proceed {
                    state,
                    target_exposure,
                },
            effects,
        } = recovery
        else {
            panic!("expected matching request to keep proceed resolution");
        };
        assert!(effects.is_empty());
        assert_eq!(target_exposure, Exposure(4.0));
        assert_eq!(
            state.active_round
                .as_ref()
                .map(|round| round.target_exposure.clone()),
            Some(Exposure(4.0))
        );
    }

    #[test]
    fn submit_recovery_proceeds_with_pending_submit_when_latest_target_drift_is_within_min_rebalance_units_of_anchor()
     {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 0.8,
            client_order_id: "track-1-small-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(2.8));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(2.8),
            current_exposure: &Exposure(2.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.5,
                Exposure(2.0),
                Exposure(3.1),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution:
                SubmitRecoveryResolution::Proceed {
                    state,
                    target_exposure,
                },
            effects,
        } = recovery
        else {
            panic!(
                "pending submit should keep proceeding when latest target drift is within min rebalance units of the active anchor"
            );
        };

        assert!(effects.is_empty());
        assert_eq!(target_exposure, Exposure(2.8));
        assert_eq!(
            state.active_round
                .as_ref()
                .map(|round| round.target_exposure.clone()),
            Some(Exposure(2.8))
        );
    }

    #[test]
    fn submit_recovery_does_not_proceed_matching_pending_submit_without_current_plan() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 96.0,
            quantity: 0.8,
            client_order_id: "track-1-small-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(2.8));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(2.8),
            current_exposure: &Exposure(2.0),
            live_order: None,
            current_plan: None,
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!(
                "missing current plan should supersede stale pending submit instead of proceeding"
            );
        };

        assert!(effects.is_empty());
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
    }

    #[test]
    fn submit_recovery_does_not_proceed_matching_pending_submit_when_current_plan_is_below_exchange_floor()
     {
        let mut rules = test_exchange_rules();
        rules.min_qty = 1.0;
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 96.0,
            quantity: 0.8,
            client_order_id: "track-1-small-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(2.8));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(2.8),
            current_exposure: &Exposure(2.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.0,
                Exposure(2.0),
                Exposure(2.2),
                96.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!(
                "unmeetable current plan should supersede stale pending submit instead of proceeding"
            );
        };

        assert!(effects.is_empty());
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
    }

    #[test]
    fn submit_recovery_replans_when_current_exposure_crosses_anchor_direction_even_if_target_drift_is_within_threshold()
     {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 1.0,
            client_order_id: "track-1-anchor".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(4.0),
            current_exposure: &Exposure(5.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.5,
                Exposure(5.0),
                Exposure(3.8),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { .. },
            effects,
        } = recovery
        else {
            panic!(
                "pending submit should be superseded when current exposure makes the anchored direction invalid"
            );
        };

        assert!(matches!(
            effects.as_slice(),
            [TrackEffect::SubmitOrder {
                request,
                target_exposure,
            }] if request.side == Side::Sell
                && request.reduce_only
                && (request.quantity - 1.2).abs() < 1e-9
                && *target_exposure == Exposure(3.8)
        ));
    }

    #[test]
    fn submit_recovery_supersedes_pending_submit_when_target_drift_crosses_min_rebalance_units() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 0.8,
            client_order_id: "track-1-small-reconcile".into(),
            reduce_only: false,
        };
        let previous_state =
            record_submit_request(&ExecutorState::empty(now), &request, Exposure(2.8));

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &request,
            target_exposure: &Exposure(2.8),
            current_exposure: &Exposure(2.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.5,
                Exposure(2.0),
                Exposure(3.4),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { .. },
            effects,
        } = recovery
        else {
            panic!("pending submit should be superseded once target drift crosses the threshold");
        };

        assert!(matches!(
            effects.as_slice(),
            [TrackEffect::SubmitOrder {
                target_exposure,
                ..
            }] if *target_exposure == Exposure(3.4)
        ));
    }

    #[test]
    fn submit_recovery_supersedes_when_pending_slot_is_already_cleared_below_min_rebalance_units() {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 0.8,
            client_order_id: "track-1-small-reconcile".into(),
            reduce_only: false,
        };

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &ExecutorState::empty(now),
            request: &request,
            target_exposure: &Exposure(0.8),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.5,
                Exposure(0.0),
                Exposure(0.4),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!(
                "cleared pending submit should be superseded when current plan is below min rebalance units"
            );
        };

        assert!(effects.is_empty());
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
    }

    #[test]
    fn submit_recovery_supersedes_stale_effect_without_new_submit_when_active_replacement_pending_is_still_within_threshold()
     {
        let rules = test_exchange_rules();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let track_id = TrackId::new("track-1");
        let instrument = test_instrument();
        let stale_request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 6.0,
            client_order_id: "track-1-stale".into(),
            reduce_only: false,
        };
        let replacement_request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 4.0,
            client_order_id: "track-1-replacement".into(),
            reduce_only: false,
        };
        let previous_state = record_submit_request(
            &ExecutorState::empty(now),
            &replacement_request,
            Exposure(4.0),
        );

        let recovery = recover_submit_effect(SubmitRecoveryInput {
            exchange_rules: &rules,
            previous_state: &previous_state,
            request: &stale_request,
            target_exposure: &Exposure(6.0),
            current_exposure: &Exposure(0.0),
            live_order: None,
            current_plan: Some(submit_intent_input(
                &track_id,
                &instrument,
                &rules,
                1.0,
                0.5,
                Exposure(0.0),
                Exposure(4.2),
                90.0,
                now,
            )),
        });

        let SubmitRecoveryPlan {
            resolution: SubmitRecoveryResolution::Superseded { state },
            effects,
        } = recovery
        else {
            panic!(
                "stale submit effect should be superseded without generating a third submit when the active replacement is still within threshold"
            );
        };

        assert!(effects.is_empty());
        assert_eq!(state.slots, previous_state.slots);
    }

    #[test]
    fn recovery_clears_receipt_backed_slot_without_matching_pending_submit_effect() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            min_rebalance_units: 0.5,
            previous_state: Some(&previous_state),
            live_orders: &[],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected stale receipt-backed slot to be cleared");
        };
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
        assert!(state.diagnostics.recovery_anomaly.is_none());
    }

    #[test]
    fn recovery_marks_anomaly_when_pending_receipt_backed_slot_has_no_live_order() {
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let request = OrderRequest {
            instrument: test_instrument(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            client_order_id: "client-1".into(),
            reduce_only: false,
        };
        let SubmitReceiptResolution::Recorded {
            state: previous_state,
        } = record_submit_receipt(
            &record_submit_request(&ExecutorState::empty(now), &request, Exposure(4.0)),
            &request,
            Exposure(4.0),
            &OrderReceipt {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                status: OrderStatus::New,
            },
        )
        else {
            panic!("expected initial receipt to be recorded");
        };
        let pending_submit_hints = vec![PendingSubmitHint {
            request: request.clone(),
            target_exposure: Exposure(4.0),
        }];

        let recovery = recover_working_orders(RecoveryInput {
            current_exposure: &Exposure(2.0),
            target_exposure: Some(&Exposure(4.0)),
            min_rebalance_units: 0.5,
            previous_state: Some(&previous_state),
            live_orders: &[],
            pending_submit_hints: &pending_submit_hints,
            observed_at: now,
        });

        assert!(matches!(
            recovery,
            RecoveryResolution::Anomaly {
                anomaly: RecoveryAnomaly::UnknownLiveOrder,
                ..
            }
        ));
    }
}
