use serde::{Deserialize, Serialize};

use chrono::{DateTime, Utc};
use grid_core::events::ReplacementGateReason;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::execution_plan::ExecutionAction;
use crate::execution_plan::{is_meetable_minimum, round_to_step};
use crate::grid::{GridId, Instrument};
use crate::observation::OrderObservation;
use crate::ports::{OrderRequest, OrderStatus};
use crate::runtime::{
    ExecutionSlot, ExecutionStats, ExecutorState, SlotState, SubmitRecoveryAnchor, WorkingOrder,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAnomaly {
    UnknownLiveOrder,
    DuplicateLiveOrders,
    AmbiguousLiveOrder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderRole {
    IncreaseInventory,
    DecreaseInventory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderSlot(pub String);

impl OrderSlot {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesiredOrder {
    pub slot: OrderSlot,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub role: OrderRole,
}

pub struct ExecutorInput<'a> {
    pub grid_id: &'a GridId,
    pub instrument: &'a Instrument,
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub current_exposure: Exposure,
    pub target_exposure: Exposure,
    pub reference_price: f64,
    pub executor_state: Option<&'a ExecutorState>,
    pub observed_at: DateTime<Utc>,
}

pub struct ExecutorPlan {
    pub state: ExecutorState,
    pub desired_orders: Vec<DesiredOrder>,
    pub effects: Vec<ExecutionAction>,
    pub replacement_gate_reason: Option<ReplacementGateReason>,
}

pub struct RecoveryInput<'a> {
    pub exchange_rules: &'a ExchangeRules,
    pub base_qty_per_unit: f64,
    pub current_exposure: &'a Exposure,
    pub target_exposure: Option<&'a Exposure>,
    pub reference_price: Option<f64>,
    pub previous_state: Option<&'a ExecutorState>,
    pub live_orders: &'a [OrderObservation],
    pub submit_recovery_anchor: Option<&'a SubmitRecoveryAnchor>,
    pub observed_at: DateTime<Utc>,
}

pub enum RecoveryResolution {
    Rebuilt { state: ExecutorState },
    Anomaly(RecoveryAnomaly),
}

const REBALANCE_GAP_THRESHOLD: f64 = 2.0;
const CATCH_UP_GAP_THRESHOLD: f64 = 5.0;
const REBALANCE_AGE_MS: i64 = 60_000;
const CATCH_UP_AGE_MS: i64 = 180_000;
const REPLACEMENT_SAFETY_BUFFER_BPS: f64 = 5.0;
const BINANCE_TAKER_FEE_RATE: f64 = 0.0004;
const BPS_DENOMINATOR: f64 = 10_000.0;
const INVENTORY_CORE_SLOT: &str = "inventory_core";

pub fn plan(input: ExecutorInput<'_>) -> ExecutorPlan {
    let inventory_gap = input.current_exposure.delta(&input.target_exposure);
    let gap_started_at =
        resolve_gap_started_at(input.executor_state, &inventory_gap, input.observed_at);
    let gap_age_ms = gap_started_at
        .map(|started_at| (input.observed_at - started_at).num_milliseconds().max(0))
        .unwrap_or(0);
    let mode = resolve_mode(&inventory_gap, gap_age_ms);
    let last_execution_reason = resolve_reason(input.executor_state, &mode);
    let stats = update_stats(
        input.executor_state,
        input.observed_at,
        &inventory_gap,
        gap_age_ms,
    );
    let desired_orders = plan_desired_orders(&input, &inventory_gap);
    let (effects, slots, replacement_gate_reason) = diff_desired_orders(&input, &desired_orders);

    ExecutorPlan {
        state: ExecutorState {
            mode,
            inventory_gap,
            gap_started_at,
            last_reprice_at: if effects.iter().any(|effect| {
                matches!(
                    effect,
                    ExecutionAction::SubmitOrder { .. } | ExecutionAction::CancelOrder { .. }
                )
            }) {
                Some(input.observed_at)
            } else {
                input.executor_state.and_then(|state| state.last_reprice_at)
            },
            slots,
            last_execution_reason,
            recovery_anomaly: None,
            stats,
        },
        desired_orders,
        effects,
        replacement_gate_reason,
    }
}

pub fn recover_working_orders(input: RecoveryInput<'_>) -> RecoveryResolution {
    if input.live_orders.len() > 1 {
        return RecoveryResolution::Anomaly(RecoveryAnomaly::DuplicateLiveOrders);
    }

    let base_state = input
        .previous_state
        .cloned()
        .unwrap_or_else(|| ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: input
                .current_exposure
                .delta(input.target_exposure.unwrap_or(input.current_exposure)),
            gap_started_at: None,
            last_reprice_at: None,
            slots: Vec::new(),
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: input.observed_at,
                max_inventory_gap_abs: Exposure(0.0),
                max_gap_age_ms: 0,
            },
        });

    if input.live_orders.is_empty() {
        let preserved_slots = input
            .submit_recovery_anchor
            .and_then(|anchor| {
                (!base_state.slots.is_empty()
                    && base_state.slots.iter().any(|slot| {
                        slot.working_order
                            .as_ref()
                            .map(|order| order.client_order_id == anchor.client_order_id)
                            .unwrap_or(false)
                    }))
                .then(|| base_state.slots.clone())
            })
            .unwrap_or_default();

        let mut state = base_state;
        state.slots = preserved_slots;
        state.recovery_anomaly = None;
        return RecoveryResolution::Rebuilt { state };
    }

    let candidate_slot = base_state
        .slots
        .first()
        .cloned()
        .or_else(|| infer_recovery_slot(&input))
        .or_else(|| {
            (input.target_exposure.is_none() && input.reference_price.is_none()).then(|| {
                ExecutionSlot {
                    slot: OrderSlot::new(INVENTORY_CORE_SLOT),
                    state: SlotState::Empty,
                    working_order: None,
                }
            })
        });

    let Some(mut slot) = candidate_slot else {
        return RecoveryResolution::Anomaly(RecoveryAnomaly::UnknownLiveOrder);
    };

    let live_order = &input.live_orders[0];
    let expected_side = slot.working_order.as_ref().map(|order| order.side);
    if expected_side.is_some() && expected_side != Some(live_order.side) {
        return RecoveryResolution::Anomaly(RecoveryAnomaly::AmbiguousLiveOrder);
    }

    let target_exposure = slot
        .working_order
        .as_ref()
        .map(|order| order.target_exposure.clone())
        .or_else(|| input.target_exposure.cloned())
        .unwrap_or_else(|| input.current_exposure.clone());
    let role = slot
        .working_order
        .as_ref()
        .map(|order| order.role.clone())
        .unwrap_or_else(|| match live_order.side {
            Side::Buy => OrderRole::IncreaseInventory,
            Side::Sell => OrderRole::DecreaseInventory,
        });
    slot.state = SlotState::Working;
    slot.working_order = Some(WorkingOrder {
        order_id: Some(live_order.order_id.clone()),
        client_order_id: live_order.client_order_id.clone(),
        side: live_order.side,
        price: live_order.price,
        quantity: live_order.quantity,
        target_exposure,
        status: live_order.status,
        role,
    });

    let mut state = base_state;
    state.slots = vec![slot];
    state.recovery_anomaly = None;
    RecoveryResolution::Rebuilt { state }
}

fn infer_recovery_slot(input: &RecoveryInput<'_>) -> Option<ExecutionSlot> {
    let target_exposure = input.target_exposure?;
    let reference_price = input.reference_price?;
    desired_inventory_order(
        input.exchange_rules,
        input.base_qty_per_unit,
        input.current_exposure,
        target_exposure,
        reference_price,
    )
    .map(|desired_order| ExecutionSlot {
        slot: desired_order.slot.clone(),
        state: SlotState::Empty,
        working_order: Some(WorkingOrder {
            order_id: None,
            client_order_id: String::new(),
            side: desired_order.side,
            price: desired_order.price,
            quantity: desired_order.quantity,
            target_exposure: desired_order.target_exposure.clone(),
            status: OrderStatus::Submitting,
            role: desired_order.role.clone(),
        }),
    })
}

fn resolve_gap_started_at(
    previous_state: Option<&ExecutorState>,
    inventory_gap: &Exposure,
    observed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if inventory_gap.is_zero() {
        return None;
    }

    previous_state
        .and_then(|state| {
            (!state.inventory_gap.is_zero()
                && state.inventory_gap.0.signum() == inventory_gap.0.signum())
            .then_some(state.gap_started_at)
            .flatten()
        })
        .or(Some(observed_at))
}

fn resolve_mode(inventory_gap: &Exposure, gap_age_ms: i64) -> ExecutionMode {
    let abs_gap = inventory_gap.0.abs();
    if abs_gap >= CATCH_UP_GAP_THRESHOLD || gap_age_ms >= CATCH_UP_AGE_MS {
        return ExecutionMode::CatchUp;
    }
    if abs_gap >= REBALANCE_GAP_THRESHOLD || gap_age_ms >= REBALANCE_AGE_MS {
        return ExecutionMode::Rebalance;
    }
    ExecutionMode::Passive
}

fn resolve_reason(
    previous_state: Option<&ExecutorState>,
    mode: &ExecutionMode,
) -> Option<ExecutionReason> {
    let previous_mode = previous_state.map(|state| &state.mode);
    if previous_mode == Some(mode) {
        return previous_state.and_then(|state| state.last_execution_reason.clone());
    }

    Some(match mode {
        ExecutionMode::Passive => ExecutionReason::GapEnteredPassive,
        ExecutionMode::Rebalance => ExecutionReason::GapEscalatedToRebalance,
        ExecutionMode::CatchUp => ExecutionReason::GapEscalatedToCatchUp,
    })
}

fn update_stats(
    previous_state: Option<&ExecutorState>,
    observed_at: DateTime<Utc>,
    inventory_gap: &Exposure,
    gap_age_ms: i64,
) -> ExecutionStats {
    let started_at = previous_state
        .map(|state| state.stats.started_at)
        .unwrap_or(observed_at);
    let previous_max_gap = previous_state
        .map(|state| state.stats.max_inventory_gap_abs.0.abs())
        .unwrap_or(0.0);
    let previous_max_age = previous_state
        .map(|state| state.stats.max_gap_age_ms)
        .unwrap_or(0);

    ExecutionStats {
        started_at,
        max_inventory_gap_abs: Exposure(previous_max_gap.max(inventory_gap.0.abs())),
        max_gap_age_ms: previous_max_age.max(gap_age_ms),
    }
}

fn plan_desired_orders(input: &ExecutorInput<'_>, _inventory_gap: &Exposure) -> Vec<DesiredOrder> {
    desired_inventory_order(
        input.exchange_rules,
        input.base_qty_per_unit,
        &input.current_exposure,
        &input.target_exposure,
        input.reference_price,
    )
    .into_iter()
    .collect()
}

fn desired_inventory_order(
    exchange_rules: &ExchangeRules,
    base_qty_per_unit: f64,
    current_exposure: &Exposure,
    target_exposure: &Exposure,
    reference_price: f64,
) -> Option<DesiredOrder> {
    let inventory_gap = current_exposure.delta(target_exposure);
    let side = Side::from_exposure(&inventory_gap)?;
    let price = round_to_step(reference_price, exchange_rules.price_tick);
    let quantity = round_to_step(
        inventory_gap.0.abs() * base_qty_per_unit,
        exchange_rules.quantity_step,
    );
    if !is_meetable_minimum(price, quantity, exchange_rules) {
        return None;
    }

    Some(DesiredOrder {
        slot: OrderSlot::new(INVENTORY_CORE_SLOT),
        side,
        price,
        quantity,
        target_exposure: target_exposure.clone(),
        role: match side {
            Side::Buy => OrderRole::IncreaseInventory,
            Side::Sell => OrderRole::DecreaseInventory,
        },
    })
}

fn diff_desired_orders(
    input: &ExecutorInput<'_>,
    desired_orders: &[DesiredOrder],
) -> (
    Vec<ExecutionAction>,
    Vec<ExecutionSlot>,
    Option<ReplacementGateReason>,
) {
    let current_slot = input
        .executor_state
        .and_then(|state| {
            state
                .slots
                .iter()
                .find(|slot| slot.slot.0 == INVENTORY_CORE_SLOT)
        })
        .cloned();
    let desired_order = desired_orders.first();

    match (current_slot, desired_order) {
        (None, None) => (vec![ExecutionAction::NoOp], Vec::new(), None),
        (Some(current_slot), None) => {
            if let Some(order_id) = current_slot
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.clone())
            {
                return (
                    vec![ExecutionAction::CancelOrder {
                        instrument: input.instrument.clone(),
                        order_id,
                    }],
                    Vec::new(),
                    None,
                );
            }
            (vec![ExecutionAction::NoOp], Vec::new(), None)
        }
        (None, Some(desired_order)) => {
            let request = desired_order_to_request(input, desired_order);
            (
                vec![ExecutionAction::SubmitOrder {
                    request: request.clone(),
                    target_exposure: desired_order.target_exposure.clone(),
                }],
                vec![submit_pending_slot(desired_order, &request)],
                None,
            )
        }
        (Some(current_slot), Some(desired_order)) => {
            let current_order = current_slot.working_order.as_ref();
            if let Some(current_order) = current_order {
                if desired_matches_working_order(desired_order, current_order, input.exchange_rules)
                {
                    return (
                        vec![ExecutionAction::NoOp],
                        vec![current_slot],
                        Some(ReplacementGateReason::RoundedMatch),
                    );
                }

                if let Some(reason) = replacement_gate_reason_for_working_order(
                    current_order,
                    desired_order,
                    input.reference_price,
                    input.exchange_rules,
                ) {
                    return (
                        vec![ExecutionAction::NoOp],
                        vec![current_slot],
                        Some(reason),
                    );
                }

                if let Some(order_id) = current_order.order_id.clone() {
                    let request = desired_order_to_request(input, desired_order);
                    return (
                        vec![
                            ExecutionAction::CancelOrder {
                                instrument: input.instrument.clone(),
                                order_id,
                            },
                            ExecutionAction::SubmitOrder {
                                request: request.clone(),
                                target_exposure: desired_order.target_exposure.clone(),
                            },
                        ],
                        vec![submit_pending_slot(desired_order, &request)],
                        None,
                    );
                }
            }

            (vec![ExecutionAction::NoOp], vec![current_slot], None)
        }
    }
}

fn desired_order_to_request(
    input: &ExecutorInput<'_>,
    desired_order: &DesiredOrder,
) -> OrderRequest {
    OrderRequest {
        instrument: input.instrument.clone(),
        side: desired_order.side,
        price: desired_order.price,
        quantity: desired_order.quantity,
        client_order_id: format!("{}-reconcile", input.grid_id.as_str()),
    }
}

fn submit_pending_slot(desired_order: &DesiredOrder, request: &OrderRequest) -> ExecutionSlot {
    ExecutionSlot {
        slot: desired_order.slot.clone(),
        state: SlotState::SubmitPending,
        working_order: Some(WorkingOrder {
            order_id: None,
            client_order_id: request.client_order_id.clone(),
            side: desired_order.side,
            price: desired_order.price,
            quantity: desired_order.quantity,
            target_exposure: desired_order.target_exposure.clone(),
            status: OrderStatus::Submitting,
            role: desired_order.role.clone(),
        }),
    }
}

fn desired_matches_working_order(
    desired_order: &DesiredOrder,
    current_order: &WorkingOrder,
    rules: &ExchangeRules,
) -> bool {
    desired_order.side == current_order.side
        && desired_order.role == current_order.role
        && rounded_values_match(desired_order.price, current_order.price, rules.price_tick)
        && rounded_values_match(
            desired_order.quantity,
            current_order.quantity,
            rules.quantity_step,
        )
}

fn replacement_gate_reason_for_working_order(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
    rules: &ExchangeRules,
) -> Option<ReplacementGateReason> {
    if current_order.order_id.is_none() {
        return None;
    }
    if current_order.side != desired_order.side {
        return None;
    }
    if !rounded_values_match(
        current_order.quantity,
        desired_order.quantity,
        rules.quantity_step,
    ) {
        return None;
    }

    let improvement_ratio =
        replacement_improvement_ratio(current_order, desired_order, reference_price);
    let threshold_rate =
        (BINANCE_TAKER_FEE_RATE * 2.0) + (REPLACEMENT_SAFETY_BUFFER_BPS / BPS_DENOMINATOR);
    (improvement_ratio < threshold_rate).then(|| ReplacementGateReason::ImprovementBelowThreshold {
        improvement_bps: ratio_to_bps(improvement_ratio),
        threshold_bps: ratio_to_bps(threshold_rate),
    })
}

fn replacement_improvement_ratio(
    current_order: &WorkingOrder,
    desired_order: &DesiredOrder,
    reference_price: f64,
) -> f64 {
    let price_improvement = match desired_order.side {
        Side::Buy => current_order.price - desired_order.price,
        Side::Sell => desired_order.price - current_order.price,
    };
    if price_improvement <= 0.0 || reference_price <= f64::EPSILON {
        return 0.0;
    }
    price_improvement / reference_price
}

fn rounded_values_match(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        (step * 1e-9).max(f64::EPSILON * 16.0)
    };
    (left - right).abs() <= tolerance
}

fn ratio_to_bps(ratio: f64) -> f64 {
    ((ratio * BPS_DENOMINATOR) * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::grid::GridId;
    use crate::grid::Venue;
    use crate::ports::OrderStatus;
    use crate::runtime::{ExecutionSlot, ExecutionStats, SlotState, WorkingOrder};

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
}
