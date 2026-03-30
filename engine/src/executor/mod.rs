use serde::{Deserialize, Serialize};

mod planning;
mod recording;
mod recovery;
mod slots;

pub(crate) use planning::{DesiredOrder, ExecutorInput, current_submit_hint, plan, refresh_state};
pub use planning::{OrderRole, OrderSlot, PendingSubmitHint};
pub(crate) use recording::{
    SubmitReceiptResolution, apply_order_observation, clear_all_working_orders,
    clear_working_order_by_order_id, record_submit_failure, record_submit_receipt,
    record_submit_request,
};
pub use recovery::{RecoveryAnomaly, SubmitRecoveryPlan, SubmitRecoveryResolution};
pub(crate) use recovery::{
    RecoveryInput, RecoveryResolution, SubmitRecoveryInput, SubmitRecoveryPlanContext,
    recover_submit_effect, recover_working_orders, submit_requests_match,
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
    use grid_core::events::ReplacementGateReason;
    use grid_core::types::{ExchangeRules, Exposure, Side};

    use super::*;
    use crate::execution_plan::ExecutionAction;
    use crate::grid::{GridId, Instrument, Venue};
    use crate::observation::OrderObservation;
    use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};
    use crate::runtime::{ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder};

    fn test_grid_id() -> GridId {
        GridId::new("btc-core")
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

    fn test_executor_state(
        mode: ExecutionMode,
        gap_started_at: Option<DateTime<Utc>>,
    ) -> ExecutorState {
        ExecutorState {
            mode,
            inventory_gap: Exposure(4.0),
            gap_started_at,
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    target_exposure: Exposure(4.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: None,
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
                target_exposure: Exposure(2.0),
                status: OrderStatus::PartiallyFilled,
                role: OrderRole::DecreaseInventory,
            }),
        }
    }

    #[test]
    fn plans_execution_mode_from_gap_and_age() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let passive = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(1.0),
            reference_price: 99.9,
            executor_state: None,
            observed_at: now,
        });
        assert_eq!(passive.state.mode, ExecutionMode::Passive);
        assert_eq!(
            passive.state.last_execution_reason,
            Some(ExecutionReason::GapEnteredPassive)
        );

        let rebalance = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(3.0),
            reference_price: 99.9,
            executor_state: Some(&test_executor_state(
                ExecutionMode::Passive,
                Some(now - Duration::seconds(90)),
            )),
            observed_at: now,
        });
        assert_eq!(rebalance.state.mode, ExecutionMode::Rebalance);
        assert_eq!(
            rebalance.state.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToRebalance)
        );

        let catch_up = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(6.0),
            reference_price: 99.9,
            executor_state: Some(&test_executor_state(
                ExecutionMode::Rebalance,
                Some(now - Duration::seconds(240)),
            )),
            observed_at: now,
        });
        assert_eq!(catch_up.state.mode, ExecutionMode::CatchUp);
        assert_eq!(
            catch_up.state.last_execution_reason,
            Some(ExecutionReason::GapEscalatedToCatchUp)
        );
        assert_eq!(catch_up.desired_orders.len(), 1);
        assert!(
            catch_up.state.stats.max_inventory_gap_abs.0 >= catch_up.state.inventory_gap.0.abs()
        );
    }

    #[test]
    fn cancel_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let mut existing_state = test_executor_state(ExecutionMode::Passive, Some(now));
        existing_state.slots.push(sibling_slot());

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(4.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        assert!(matches!(
            plan.effects.as_slice(),
            [ExecutionAction::CancelOrder { order_id, .. }] if order_id == "order-1"
        ));
        assert_eq!(plan.state.slots, existing_state.slots);
    }

    #[test]
    fn replace_plan_keeps_live_slot_until_cancel_effect_completes() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 90.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

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
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let hint = current_submit_hint(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: now,
        });

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
    fn current_submit_hint_returns_none_when_plan_is_not_single_submit() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let hint = current_submit_hint(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 90.0,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        assert!(hint.is_none());
    }

    #[test]
    fn plan_sets_reduce_only_for_decrease_inventory_order() {
        let instrument = test_instrument();
        let rules = test_exchange_rules();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(6.0),
            target_exposure: Exposure(2.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: now,
        });

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
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();

        let plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: now,
        });

        let submit = plan.effects.iter().find_map(|effect| match effect {
            ExecutionAction::SubmitOrder { request, .. } => Some(request),
            _ => None,
        });
        assert!(submit.is_some());
        assert!(!submit.unwrap().reduce_only);
    }

    #[test]
    fn replacement_gate_threshold_uses_exchange_maker_and_taker_fee_rate() {
        let instrument = test_instrument();
        let grid_id = test_grid_id();
        let now = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let existing_state = test_executor_state(ExecutionMode::Passive, Some(now));

        let low_fee_rules = test_exchange_rules();
        let low_fee_plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &low_fee_rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 94.9,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

        let mut high_fee_rules = test_exchange_rules();
        high_fee_rules.maker_fee_rate = 0.0005;
        high_fee_rules.taker_fee_rate = 0.001;
        let high_fee_plan = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &high_fee_rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 94.9,
            executor_state: Some(&existing_state),
            observed_at: now,
        });

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
                target_exposure: Exposure(2.0),
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
        assert!(state.recovery_anomaly.is_none());
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
            state.recovery_anomaly.as_ref(),
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
        let grid_id = GridId::new("grid-1");
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
            current_plan: Some(SubmitRecoveryPlanContext {
                grid_id: &grid_id,
                instrument: &instrument,
                base_qty_per_unit: 3.75,
                target_exposure: Exposure(4.0),
                reference_price: 95.0,
                observed_at: now,
            }),
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
                    client_order_id: format!("grid-1-{}", now.timestamp_millis()),
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
                    client_order_id: format!("grid-1-{}", now.timestamp_millis()),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    target_exposure: Exposure(4.0),
                    status: OrderStatus::Submitting,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
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
        let grid_id = test_grid_id();
        let t1 = Utc.with_ymd_and_hms(2026, 3, 29, 8, 5, 0).unwrap();
        let t2 = t1 + Duration::milliseconds(1);

        let plan1 = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: t1,
        });
        let plan2 = plan(ExecutorInput {
            grid_id: &grid_id,
            instrument: &instrument,
            exchange_rules: &rules,
            base_qty_per_unit: 3.75,
            current_exposure: Exposure(0.0),
            target_exposure: Exposure(4.0),
            reference_price: 95.0,
            executor_state: None,
            observed_at: t2,
        });

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
        let grid_id = GridId::new("grid-1");
        let instrument = test_instrument();
        let request = OrderRequest {
            instrument: instrument.clone(),
            side: Side::Buy,
            price: 90.0,
            quantity: 4.0,
            client_order_id: "grid-1-reconcile".into(),
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
            current_plan: Some(SubmitRecoveryPlanContext {
                grid_id: &grid_id,
                instrument: &instrument,
                base_qty_per_unit: 1.0,
                target_exposure: Exposure(4.0),
                reference_price: 90.0,
                observed_at: now,
            }),
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
            state.slots[0]
                .working_order
                .as_ref()
                .map(|order| order.target_exposure.clone()),
            Some(Exposure(4.0))
        );
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
            previous_state: Some(&previous_state),
            live_orders: &[],
            pending_submit_hints: &[],
            observed_at: now,
        });

        let RecoveryResolution::Rebuilt { state } = recovery else {
            panic!("expected stale receipt-backed slot to be cleared");
        };
        assert_eq!(state.slots, vec![slots::empty_inventory_core_slot()]);
        assert!(state.recovery_anomaly.is_none());
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
