use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::ExchangeRules;
use grid_core::types::Exposure;

use crate::command::GridCommand;
use crate::executor;
use crate::grid::{GridId, Instrument};
use crate::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use crate::ports::{ClockPort, ExchangeOrder, OrderReceipt, OrderRequest};
use crate::reconciler;
use crate::runtime::{ExecutorState, GridRuntime, GridStatus};
use crate::snapshot::GridRuntimeSnapshot;
use crate::transition::{GridEffect, GridTransition};

pub struct GridManager {
    grids: HashMap<GridId, GridRuntime>,
    instruments: HashMap<Instrument, GridId>,
    clock: Arc<dyn ClockPort>,
}

impl GridManager {
    pub fn new(clock: Arc<dyn ClockPort>) -> Self {
        Self {
            grids: HashMap::new(),
            instruments: HashMap::new(),
            clock,
        }
    }

    pub fn add_grid(
        &mut self,
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
    ) -> Result<()> {
        if self.grids.contains_key(&id) {
            bail!("duplicate grid id `{}`", id.as_str());
        }
        if self.instruments.contains_key(&instrument) {
            bail!(
                "duplicate instrument `{}:{}`",
                instrument.venue.as_str(),
                instrument.symbol
            );
        }

        grid_core::strategy::validate_config(&config).map_err(|e| anyhow::anyhow!(e))?;
        let grid = GridRuntime::new(
            id.clone(),
            instrument.clone(),
            config,
            budget,
            exchange_rules,
            self.clock.now(),
        );
        self.grids.insert(id.clone(), grid);
        self.instruments.insert(instrument, id);
        Ok(())
    }

    pub fn resolve_grid_id(&self, instrument: &Instrument) -> Option<GridId> {
        self.instruments.get(instrument).cloned()
    }

    pub fn observe(&mut self, id: &GridId, observation: GridObservation) -> Result<GridTransition> {
        let (events, effects) = match observation {
            GridObservation::Market(observation) => self.observe_market(id, observation)?,
            GridObservation::Position(observation) => {
                self.observe_position(id, observation)?;
                match self.cached_reference_price(id)? {
                    Some(reference_price) => self.reconcile_grid(id, reference_price)?,
                    None => (vec![], vec![]),
                }
            }
            GridObservation::Order(observation) => {
                let should_reconcile = observation.status.should_reconcile_after_order_update();
                self.observe_order(id, observation)?;
                match (should_reconcile, self.cached_reference_price(id)?) {
                    (true, Some(reference_price)) => self.reconcile_grid(id, reference_price)?,
                    _ => (vec![], vec![]),
                }
            }
        };

        self.transition_for(id, events, effects)
    }

    pub fn sync_exchange_state(
        &mut self,
        id: &GridId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        pending_submit_hints: Vec<executor::PendingSubmitHint>,
    ) -> Result<GridTransition> {
        let (events, effects) =
            self.apply_startup_exchange_state(id, position, open_orders, pending_submit_hints)?;
        self.transition_for(id, events, effects)
    }

    pub fn command(&mut self, id: &GridId, command: GridCommand) -> Result<GridTransition> {
        let (events, effects) = match command {
            GridCommand::Pause => {
                self.pause_grid(id.as_str())?;
                (vec![], vec![])
            }
            GridCommand::Resume => {
                self.resume_grid(id.as_str())?;
                (vec![], vec![])
            }
            GridCommand::Reconcile => {
                let Some(reference_price) = self
                    .grids
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?
                    .reference_price
                else {
                    return self.transition_for(id, vec![], vec![]);
                };
                self.reconcile_grid(id, reference_price)?
            }
        };

        self.transition_for(id, events, effects)
    }

    pub fn pause_grid(&mut self, id: &str) -> Result<()> {
        let grid = self
            .grids
            .get_mut(&GridId::from(id))
            .ok_or_else(|| anyhow::anyhow!("grid `{id}` not found"))?;
        // Pause disables strategy targeting, but does not rewrite observed exchange state.
        grid.status = GridStatus::Paused;
        grid.target_exposure = None;
        Ok(())
    }

    pub fn resume_grid(&mut self, id: &str) -> Result<()> {
        let resumed_at = self.clock.now();
        let resumed_state = {
            let grid = self
                .grids
                .get(&GridId::from(id))
                .ok_or_else(|| anyhow::anyhow!("grid `{id}` not found"))?;

            if !matches!(grid.status, GridStatus::Paused) {
                bail!("cannot resume grid `{id}` from status {:?}", grid.status);
            }

            if let Some(reference_price) = grid.reference_price {
                let mut resumed = grid.clone();
                resumed.status = GridStatus::WaitingMarketData;
                resumed.executor_state = grid.executor_state.reset_for_activation(resumed_at);
                let result = self.plan_inventory_execution_for_grid(&resumed, reference_price)?;
                (
                    result.new_status.unwrap_or(GridStatus::Active),
                    Some(result.target_exposure.clone()),
                    result.replacement_gate_reason,
                    executor::refresh_state(
                        &resumed.executor_state,
                        &resumed.current_exposure,
                        &result.target_exposure,
                        resumed_at,
                    ),
                )
            } else {
                (
                    GridStatus::WaitingMarketData,
                    None,
                    None,
                    grid.executor_state.reset_for_activation(resumed_at),
                )
            }
        };

        let grid = self
            .grids
            .get_mut(&GridId::from(id))
            .ok_or_else(|| anyhow::anyhow!("grid `{id}` not found"))?;
        let (status, exposure, replacement_gate_reason, executor_state) = resumed_state;
        grid.status = status;
        grid.target_exposure = exposure;
        grid.replacement_gate_reason = replacement_gate_reason;
        grid.executor_state = executor_state;

        Ok(())
    }

    pub fn snapshot(&self, id: &str) -> Option<GridRuntimeSnapshot> {
        self.get_grid(id).map(GridRuntime::snapshot)
    }

    pub fn restore_grid_state(&mut self, snapshot: &GridRuntimeSnapshot) -> Result<()> {
        let grid = self
            .grids
            .get_mut(&snapshot.grid_id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", snapshot.grid_id.as_str()))?;
        if grid.instrument != snapshot.instrument {
            bail!(
                "snapshot instrument mismatch for `{}`: expected `{}:{}`, got `{}:{}`",
                snapshot.grid_id.as_str(),
                grid.instrument.venue.as_str(),
                grid.instrument.symbol,
                snapshot.instrument.venue.as_str(),
                snapshot.instrument.symbol
            );
        }
        grid.restore_from_snapshot(snapshot)?;
        Ok(())
    }

    pub fn record_submit_request(
        &mut self,
        id: &GridId,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let next_state =
            executor::record_submit_request(&grid.executor_state, request, target_exposure);
        if next_state != grid.executor_state {
            grid.executor_state = next_state;
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_submit_receipt(
        &mut self,
        id: &GridId,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let resolution = executor::record_submit_receipt(
            &grid.executor_state,
            request,
            target_exposure,
            receipt,
        );
        match resolution {
            executor::SubmitReceiptResolution::Recorded { state } => {
                if state != grid.executor_state {
                    grid.executor_state = state;
                    grid.replacement_gate_reason = None;
                }
                Ok(())
            }
            executor::SubmitReceiptResolution::Unmatched => bail!(
                "submit receipt did not match executor slot: grid=`{}`, client_order_id=`{}`, order_id=`{}`",
                id.as_str(),
                request.client_order_id,
                receipt.order_id,
            ),
        }
    }

    pub fn record_submit_failure(&mut self, id: &GridId, client_order_id: &str) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let next_state = executor::record_submit_failure(&grid.executor_state, client_order_id);
        if next_state != grid.executor_state {
            grid.executor_state = next_state;
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    fn clear_working_order_by_order_id(&mut self, id: &GridId, order_id: &str) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let next_state = executor::clear_working_order_by_order_id(&grid.executor_state, order_id);
        if next_state != grid.executor_state {
            grid.executor_state = next_state;
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_cancel_order_success(&mut self, id: &GridId, order_id: &str) -> Result<()> {
        self.clear_working_order_by_order_id(id, order_id)
    }

    fn clear_all_working_orders(&mut self, id: &GridId) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let next_state = executor::clear_all_working_orders(&grid.executor_state);
        if next_state != grid.executor_state {
            grid.executor_state = next_state;
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_cancel_all_success(&mut self, id: &GridId) -> Result<()> {
        self.clear_all_working_orders(id)
    }

    pub fn recover_submit_effect(
        &mut self,
        id: &GridId,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<executor::SubmitRecoveryPlan> {
        let live_order_observation = live_order.map(|order| OrderObservation {
            order_id: order.order_id.clone(),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.qty,
            realized_pnl: 0.0,
            status: order.status,
        });
        let observed_at = self.clock.now();

        let plan = {
            let grid = self
                .grids
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
            let current_plan = self.submit_recovery_plan_context(grid, observed_at);
            executor::recover_submit_effect(executor::SubmitRecoveryInput {
                exchange_rules: &grid.exchange_rules,
                previous_state: &grid.executor_state,
                request,
                target_exposure: &target_exposure,
                current_exposure: &grid.current_exposure,
                live_order: live_order_observation.as_ref(),
                current_plan,
            })
        };

        {
            let grid = self
                .grids
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
            if let Some(state) = plan.resolution.state() {
                if state != &grid.executor_state {
                    grid.executor_state = state.clone();
                    grid.replacement_gate_reason = None;
                }
            }
        };

        Ok(plan)
    }

    pub fn list_grids(&self) -> Vec<&GridRuntime> {
        self.grids.values().collect()
    }

    pub fn get_grid(&self, id: &str) -> Option<&GridRuntime> {
        self.grids.get(&GridId::from(id))
    }

    pub fn clock(&self) -> &dyn ClockPort {
        self.clock.as_ref()
    }

    fn transition_for(
        &self,
        id: &GridId,
        events: Vec<DomainEvent>,
        effects: Vec<GridEffect>,
    ) -> Result<GridTransition> {
        let snapshot = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?
            .snapshot();
        Ok(GridTransition {
            snapshot,
            events,
            effects,
        })
    }

    fn cached_reference_price(&self, id: &GridId) -> Result<Option<f64>> {
        Ok(self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?
            .reference_price)
    }

    fn observe_market(
        &mut self,
        id: &GridId,
        observation: MarketObservation,
    ) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
        self.reconcile_grid(id, observation.reference_price)
    }

    fn observe_position(&mut self, id: &GridId, observation: PositionObservation) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let unit_qty = grid.config.base_qty_per_unit();
        grid.current_exposure = if unit_qty <= f64::EPSILON {
            grid_core::types::Exposure(0.0)
        } else {
            grid_core::types::Exposure(observation.qty / unit_qty)
        };
        grid.risk_state.unrealized_pnl = observation.unrealized_pnl;
        Ok(())
    }

    fn observe_order(&mut self, id: &GridId, observation: OrderObservation) -> Result<()> {
        let today = self.clock.now().date_naive();
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;

        if grid.risk_state.realized_pnl_day != Some(today) {
            grid.risk_state.realized_pnl_day = Some(today);
            grid.risk_state.realized_pnl_today = 0.0;
        }
        if observation.realized_pnl.abs() > f64::EPSILON {
            grid.risk_state.realized_pnl_today += observation.realized_pnl;
            grid.risk_state.realized_pnl_cumulative += observation.realized_pnl;
        }

        if grid.executor_state.recovery_anomaly.is_some() {
            return Ok(());
        }

        let next_state = executor::apply_order_observation(&grid.executor_state, &observation);
        if next_state != grid.executor_state {
            grid.executor_state = next_state;
            grid.replacement_gate_reason = None;
        }

        Ok(())
    }

    fn apply_startup_exchange_state(
        &mut self,
        id: &GridId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        pending_submit_hints: Vec<executor::PendingSubmitHint>,
    ) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
        self.observe_position(id, position)?;
        let observed_at = self.clock.now();
        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?
            .clone();
        let previous_state = grid.executor_state.clone();
        let recovery = executor::recover_working_orders(executor::RecoveryInput {
            exchange_rules: &grid.exchange_rules,
            base_qty_per_unit: grid.config.base_qty_per_unit(),
            current_exposure: &grid.current_exposure,
            target_exposure: grid.target_exposure.as_ref(),
            reference_price: grid.reference_price,
            previous_state: Some(&previous_state),
            live_orders: &open_orders,
            pending_submit_hints: &pending_submit_hints,
            observed_at,
        });

        match recovery {
            executor::RecoveryResolution::Anomaly { state, .. } => {
                let grid = self
                    .grids
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                grid.executor_state = state;
                grid.replacement_gate_reason = None;
                Ok((vec![], vec![GridEffect::NoOp]))
            }
            executor::RecoveryResolution::Rebuilt { state } => {
                let mut planned_grid = grid.clone();
                planned_grid.executor_state = state;

                if matches!(planned_grid.status, GridStatus::Paused) {
                    let grid = self
                        .grids
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                    grid.executor_state = planned_grid.executor_state;
                    grid.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                }

                let Some(reference_price) = planned_grid.reference_price else {
                    let grid = self
                        .grids
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                    grid.executor_state = planned_grid.executor_state;
                    grid.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                };

                let planned =
                    self.plan_inventory_execution_for_grid(&planned_grid, reference_price)?;
                let effects = planned
                    .effects
                    .iter()
                    .filter(|effect| match effect {
                        GridEffect::SubmitOrder { request, .. } => {
                            !pending_submit_hints.iter().any(|hint| {
                                executor::submit_requests_match(
                                    &hint.request,
                                    request,
                                    &planned_grid.exchange_rules,
                                )
                            })
                        }
                        _ => true,
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let grid = self
                    .grids
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                if let Some(new_status) = planned.new_status {
                    grid.status = new_status;
                }
                grid.target_exposure = Some(planned.target_exposure);
                grid.reference_price = Some(reference_price);
                grid.replacement_gate_reason = planned.replacement_gate_reason;
                grid.executor_state = planned.executor_state;
                Ok((planned.events, effects))
            }
        }
    }

    fn reconcile_grid(
        &mut self,
        id: &GridId,
        reference_price: f64,
    ) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
        if matches!(self.grids[&id].status, GridStatus::Paused) {
            let grid = self.grids.get_mut(id).unwrap();
            grid.reference_price = Some(reference_price);
            grid.target_exposure = None;
            grid.replacement_gate_reason = None;
            return Ok((vec![], vec![]));
        }

        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let PlannedInventoryExecution {
            mut events,
            effects: planned_effects,
            target_exposure,
            new_status,
            replacement_gate_reason,
            executor_state,
        } = self.plan_inventory_execution_for_grid(grid, reference_price)?;
        let effects = planned_effects;

        let grid = self.grids.get_mut(id).unwrap();
        let replacement_gate_event = (grid.replacement_gate_reason != replacement_gate_reason)
            .then(|| replacement_gate_reason.clone())
            .flatten()
            .map(|reason| DomainEvent::ReplacementGateApplied { reason });
        if let Some(new_status) = new_status {
            grid.status = new_status;
        }
        grid.target_exposure = Some(target_exposure);
        grid.reference_price = Some(reference_price);
        grid.replacement_gate_reason = replacement_gate_reason;
        grid.executor_state = executor_state;

        if let Some(event) = replacement_gate_event {
            events.push(event);
        }

        Ok((events, effects))
    }

    fn submit_recovery_plan_context<'a>(
        &self,
        grid: &'a GridRuntime,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<executor::SubmitRecoveryPlanContext<'a>> {
        let reference_price = grid.reference_price?;
        if matches!(grid.status, GridStatus::Paused) {
            return None;
        }

        let target = reconciler::reconcile_target(grid, reference_price);
        (!target.suppress_execution).then_some(executor::SubmitRecoveryPlanContext {
            grid_id: &grid.id,
            instrument: &grid.instrument,
            base_qty_per_unit: grid.config.base_qty_per_unit(),
            target_exposure: target.target_exposure,
            reference_price,
            observed_at,
        })
    }

    fn plan_inventory_execution_for_grid(
        &self,
        grid: &GridRuntime,
        reference_price: f64,
    ) -> Result<PlannedInventoryExecution> {
        let target = reconciler::reconcile_target(grid, reference_price);
        if grid.executor_state.recovery_anomaly.is_some() {
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![GridEffect::NoOp],
                target_exposure: target.target_exposure,
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state: grid.executor_state.clone(),
            });
        }
        let observed_at = self.clock.now();
        if target.suppress_execution {
            let executor_state = executor::refresh_state(
                &grid.executor_state,
                &grid.current_exposure,
                &target.target_exposure,
                observed_at,
            );
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![GridEffect::NoOp],
                target_exposure: target.target_exposure.clone(),
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state,
            });
        }
        let executor_state = Some(&grid.executor_state);
        let plan = executor::plan(executor::ExecutorInput {
            grid_id: &grid.id,
            instrument: &grid.instrument,
            exchange_rules: &grid.exchange_rules,
            base_qty_per_unit: grid.config.base_qty_per_unit(),
            current_exposure: grid.current_exposure.clone(),
            target_exposure: target.target_exposure.clone(),
            reference_price,
            executor_state,
            observed_at,
        });

        Ok(PlannedInventoryExecution {
            events: target.events,
            effects: plan.effects,
            target_exposure: target.target_exposure,
            new_status: target.new_status,
            replacement_gate_reason: plan.replacement_gate_reason,
            executor_state: plan.state,
        })
    }
}

struct PlannedInventoryExecution {
    events: Vec<DomainEvent>,
    effects: Vec<GridEffect>,
    target_exposure: Exposure,
    new_status: Option<GridStatus>,
    replacement_gate_reason: Option<grid_core::events::ReplacementGateReason>,
    executor_state: ExecutorState,
}

#[cfg(test)]
fn rounded_values_match(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        (step * 1e-9).max(f64::EPSILON * 16.0)
    };
    (left - right).abs() <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::ports::*;
    use crate::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, GridStatus, RiskState, SlotState,
        WorkingOrder,
    };
    use chrono::{TimeZone, Utc};
    use grid_core::events::ReplacementGateReason;
    use grid_core::strategy::*;
    use grid_core::types::Side;

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    fn test_manager() -> GridManager {
        GridManager::new(Arc::new(FakeClock))
    }

    fn test_manager_with_clock(clock: Arc<dyn ClockPort>) -> GridManager {
        GridManager::new(clock)
    }

    fn test_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    fn budget_with_max_notional(max_notional: f64) -> CapacityBudget {
        CapacityBudget {
            max_notional,
            ..test_budget()
        }
    }

    fn test_exchange_rules() -> grid_core::types::ExchangeRules {
        grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
        }
    }

    fn test_instrument(symbol: &str) -> Instrument {
        Instrument::new(crate::grid::Venue::Binance, symbol)
    }

    fn register_test_grid(manager: &mut GridManager, id: &str, symbol: &str) {
        manager
            .add_grid(
                GridId::new(id),
                test_instrument(symbol),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
    }

    fn working_order(
        order_id: Option<&str>,
        client_order_id: &str,
        side: grid_core::types::Side,
        price: f64,
        quantity: f64,
        target_exposure: grid_core::types::Exposure,
        status: OrderStatus,
    ) -> WorkingOrder {
        WorkingOrder {
            order_id: order_id.map(str::to_string),
            client_order_id: client_order_id.to_string(),
            side,
            price,
            quantity,
            target_exposure,
            status,
            role: match side {
                grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
            },
        }
    }

    fn seed_executor_slot(grid: &mut GridRuntime, order: WorkingOrder, state: SlotState) {
        grid.executor_state
            .slots
            .retain(|slot| slot.slot != OrderSlot::new("inventory_core"));
        grid.executor_state.slots.insert(
            0,
            ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state,
                working_order: Some(order),
            },
        );
    }

    fn seed_named_executor_slot(
        grid: &mut GridRuntime,
        slot_name: &str,
        order: WorkingOrder,
        state: SlotState,
    ) {
        grid.executor_state
            .slots
            .retain(|slot| slot.slot != OrderSlot::new(slot_name));
        grid.executor_state.slots.push(ExecutionSlot {
            slot: OrderSlot::new(slot_name),
            state,
            working_order: Some(order),
        });
    }

    fn working_order_from_submit_request(
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
    ) -> WorkingOrder {
        working_order(
            None,
            &request.client_order_id,
            request.side,
            request.price,
            request.quantity,
            target_exposure,
            OrderStatus::Submitting,
        )
    }

    fn inventory_core_order(grid: &GridRuntime) -> Option<&WorkingOrder> {
        grid.executor_state
            .slots
            .iter()
            .find(|slot| slot.slot == OrderSlot::new("inventory_core"))
            .and_then(|slot| slot.working_order.as_ref())
    }

    fn inventory_core_order_from_snapshot(snapshot: &GridRuntimeSnapshot) -> Option<&WorkingOrder> {
        snapshot
            .executor_state
            .slots
            .iter()
            .find(|slot| slot.slot == OrderSlot::new("inventory_core"))
            .and_then(|slot| slot.working_order.as_ref())
    }

    fn empty_inventory_core_slot() -> ExecutionSlot {
        ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }
    }

    fn active_runtime_with_executor_order() -> GridRuntime {
        let mut grid = GridRuntime::new(
            GridId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(4.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        seed_executor_slot(
            &mut grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        grid.risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 12.5,
            realized_pnl_cumulative: 17.5,
            unrealized_pnl: -3.0,
        };
        grid.reference_price = Some(95.0);
        grid.out_of_band_since = Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap());
        grid
    }

    fn passive_executor_state_with_matching_buy_order() -> ExecutorState {
        ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: grid_core::types::Exposure(4.0),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    target_exposure: grid_core::types::Exposure(4.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: grid_core::types::Exposure(4.0),
                max_gap_age_ms: 0,
            },
        }
    }

    fn test_manager_with_active_grid() -> GridManager {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc-core", "BTCUSDT");
        manager
    }

    fn test_manager_with_cached_price(reference_price: f64) -> GridManager {
        let mut manager = test_manager_with_active_grid();
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.reference_price = Some(reference_price);
        manager
    }

    #[test]
    fn add_grid_validates_config() {
        let mut manager = test_manager();
        let bad_config = GridConfig {
            lower_price: 110.0,
            upper_price: 90.0,
            ..test_config()
        };
        assert!(
            manager
                .add_grid(
                    GridId::new("test"),
                    test_instrument("BTCUSDT"),
                    bad_config,
                    test_budget(),
                    test_exchange_rules(),
                )
                .is_err()
        );
    }

    #[test]
    fn add_and_list_grids() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        register_test_grid(&mut manager, "eth1", "ETHUSDT");

        assert_eq!(manager.list_grids().len(), 2);
        assert!(manager.get_grid("btc1").is_some());
        assert!(manager.get_grid("eth1").is_some());
        assert!(manager.get_grid("nonexistent").is_none());
    }

    #[test]
    fn add_grid_stores_budget_on_runtime() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.budget, test_budget());
    }

    #[test]
    fn add_grid_initializes_executor_state_from_activation_clock() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(started_at)));

        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.get_grid("btc1").unwrap();
        let executor_state = &grid.executor_state;
        assert_eq!(executor_state.slots, vec![empty_inventory_core_slot()]);
        assert_eq!(executor_state.inventory_gap, Exposure(0.0));
        assert_eq!(executor_state.gap_started_at, None);
        assert_eq!(executor_state.stats.started_at, started_at);
    }

    #[test]
    fn add_grid_rejects_duplicate_grid_ids() {
        let mut manager = test_manager();
        let grid_id = GridId::new("btc-core");
        manager
            .add_grid(
                grid_id.clone(),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager
            .add_grid(
                grid_id,
                test_instrument("ETHUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("duplicate grid id"));
    }

    #[test]
    fn add_grid_rejects_duplicate_instruments() {
        let mut manager = test_manager();
        manager
            .add_grid(
                GridId::new("btc-core"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager
            .add_grid(
                GridId::new("btc-alt"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[test]
    fn resolve_grid_id_returns_registered_grid_id_for_instrument() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc-core", "BTCUSDT");

        assert_eq!(
            manager.resolve_grid_id(&test_instrument("BTCUSDT")),
            Some(GridId::new("btc-core"))
        );
    }

    #[test]
    fn snapshot_roundtrip_preserves_runtime_state() {
        let runtime = active_runtime_with_executor_order();
        let snapshot = runtime.snapshot();
        let mut restored = GridRuntime::new(
            GridId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        restored.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.snapshot(), snapshot);
    }

    #[test]
    fn restore_grid_state_rejects_config_mismatch() {
        let mut manager = test_manager_with_active_grid();
        let snapshot = {
            let mut runtime = GridRuntime::new(
                GridId::new("btc-core"),
                test_instrument("BTCUSDT"),
                GridConfig {
                    lower_price: 80.0,
                    ..test_config()
                },
                test_budget(),
                test_exchange_rules(),
                Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
            );
            runtime.status = GridStatus::Active;
            runtime.current_exposure = grid_core::types::Exposure(0.0);
            runtime.reference_price = Some(90.0);
            runtime.snapshot()
        };

        let error = manager.restore_grid_state(&snapshot).unwrap_err();
        assert!(error.to_string().contains("snapshot config mismatch"));
    }

    #[test]
    fn restore_from_snapshot_keeps_runtime_budget_during_reconcile() {
        let mut runtime = GridRuntime::new(
            GridId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            budget_with_max_notional(1500.0),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        runtime.status = GridStatus::Active;
        runtime.current_exposure = grid_core::types::Exposure(0.0);
        runtime.reference_price = Some(90.0);

        let snapshot = {
            let mut source = GridRuntime::new(
                GridId::new("btc-core"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
                Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
            );
            source.status = GridStatus::Active;
            source.current_exposure = grid_core::types::Exposure(0.0);
            source.reference_price = Some(90.0);
            source.snapshot()
        };

        runtime.restore_from_snapshot(&snapshot).unwrap();

        let result = crate::reconciler::reconcile_target(&runtime, 90.0);
        assert_eq!(runtime.budget.max_notional, 1500.0);
        assert_eq!(result.target_exposure, grid_core::types::Exposure(4.0));
    }

    #[test]
    fn observe_market_reconciles_and_returns_effects() {
        let mut manager = test_manager_with_active_grid();
        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                crate::observation::GridObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert!(!transition.effects.is_empty());
        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert!(!transition.events.is_empty());
    }

    #[test]
    fn observe_market_plans_through_inventory_executor() {
        let mut manager = test_manager_with_active_grid();
        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                crate::observation::GridObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert!(matches!(
            transition.effects.as_slice(),
            [GridEffect::SubmitOrder { .. }]
        ));
        assert!(!transition.snapshot.executor_state.slots.is_empty());
        assert!(
            !transition
                .effects
                .iter()
                .any(|effect| matches!(effect, GridEffect::CancelAll { .. }))
        );
    }

    #[test]
    fn executor_noop_when_working_orders_match_desired_orders() {
        let mut manager = test_manager_with_active_grid();
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.executor_state = passive_executor_state_with_matching_buy_order();

        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                crate::observation::GridObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
        let executor_state = transition.snapshot.executor_state;
        assert_eq!(executor_state.slots.len(), 1);
        assert_eq!(
            executor_state.slots,
            passive_executor_state_with_matching_buy_order().slots
        );
    }

    #[test]
    fn command_reconcile_uses_cached_reference_price() {
        let mut manager = test_manager_with_cached_price(95.0);
        {
            let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
            grid.budget = CapacityBudget {
                max_notional: 1500.0,
                ..test_budget()
            };
        }
        let transition = manager
            .command(
                &GridId::new("btc-core"),
                crate::command::GridCommand::Reconcile,
            )
            .unwrap();

        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert_eq!(
            transition
                .snapshot
                .target_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
        assert!(!transition.effects.is_empty());
    }

    #[test]
    fn observe_market_updates_grid() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();
        assert!(!transition.events.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Active);
        assert_eq!(grid.reference_price, Some(95.0));
        assert_eq!(grid.current_exposure.0, 0.0);
        assert!(grid.target_exposure.as_ref().unwrap().0 > 0.0); // should be long below center
    }

    #[test]
    fn observe_market_returns_transition_with_effects_and_events() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(!transition.effects.is_empty());
        assert!(!transition.events.is_empty());
    }

    #[test]
    fn observe_market_updates_target_without_faking_current_exposure() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(2.0);

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(!transition.events.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure.0, 2.0);
        assert_eq!(grid.target_exposure.as_ref().unwrap().0, 4.0);
        assert_eq!(grid.reference_price, Some(95.0));
    }

    #[test]
    fn resolve_grid_id_returns_none_for_unknown_instrument() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        assert_eq!(manager.resolve_grid_id(&test_instrument("ETHUSDT")), None);
    }

    #[test]
    fn paused_grid_ignores_reconcile_updates() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(transition.events.is_empty());
        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Paused);
        assert_eq!(grid.current_exposure.0, 2.0);
        assert_eq!(grid.target_exposure, None);
        assert_eq!(grid.reference_price, Some(95.0));
    }

    #[test]
    fn observe_market_keeps_submit_pending_slot_without_extra_effects() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        seed_executor_slot(
            grid,
            working_order(
                None,
                "recover-1",
                grid_core::types::Side::Buy,
                94.0,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.target_exposure,
            Some(grid_core::types::Exposure(4.0))
        );
        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order(
                None,
                "recover-1",
                grid_core::types::Side::Buy,
                94.0,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ))
        );
        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
    }

    #[test]
    fn observe_market_records_submit_pending_slot_for_new_submit_effect() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(0.0);

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        let (request, target_exposure) = match transition.effects.as_slice() {
            [
                GridEffect::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => (request, target_exposure),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order_from_submit_request(
                request,
                target_exposure.clone(),
            ))
        );
    }

    #[test]
    fn observe_market_replacement_gate_emits_event_when_reason_first_appears() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Sell,
                99.9,
                7.0,
                grid_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
        assert!(transition.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason: ReplacementGateReason::RoundedMatch,
            }
        )));
    }

    #[test]
    fn observe_market_replacement_gate_deduplicates_same_reason_across_ticks() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Sell,
                99.9,
                7.0,
                grid_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let first = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();
        let second = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert!(first.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason: ReplacementGateReason::RoundedMatch,
            }
        )));
        assert!(
            !second
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ReplacementGateApplied { .. }))
        );
        assert_eq!(
            second.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
    }

    #[test]
    fn observe_market_replacement_gate_emits_event_when_reason_changes() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(0.0);
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                100.0,
                0.1,
                grid_core::types::Exposure(0.4),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        grid.replacement_gate_reason = Some(ReplacementGateReason::RoundedMatch);

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 10.0,
                threshold_bps: 13.0,
            })
        );
        assert!(transition.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason:
                    ReplacementGateReason::ImprovementBelowThreshold {
                        improvement_bps,
                        threshold_bps,
                    },
            } if (*improvement_bps - 10.0).abs() < f64::EPSILON
                && (*threshold_bps - 13.0).abs() < f64::EPSILON
        )));
    }

    #[test]
    fn resume_grid_rejects_non_paused_status() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let error = manager.resume_grid("btc1").unwrap_err();

        assert!(error.to_string().contains("cannot resume"));
    }

    #[test]
    fn resume_grid_recomputes_status_from_last_price() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.current_exposure = grid_core::types::Exposure(8.0);
        grid.reference_price = Some(85.0);
        grid.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Frozen);
        assert_eq!(grid.current_exposure.0, 8.0);
        assert_eq!(
            grid.target_exposure.as_ref().map(|target| target.0),
            Some(4.0)
        );
    }

    #[test]
    fn resume_grid_recomputes_replacement_gate_reason_from_last_price() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.reference_price = Some(99.95);
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Sell,
                99.9,
                7.0,
                grid_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Active);
        assert_eq!(
            grid.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
    }

    #[test]
    fn resume_grid_resets_execution_stats_for_new_activation() {
        let resumed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(resumed_at)));
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.current_exposure = Exposure(2.0);
        grid.reference_price = Some(95.0);
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                Side::Buy,
                95.0,
                2.0,
                Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let executor_state = &mut grid.executor_state;
        executor_state.inventory_gap = Exposure(2.0);
        executor_state.gap_started_at = Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap());
        executor_state.stats.started_at = Utc.with_ymd_and_hms(2026, 3, 29, 7, 30, 0).unwrap();
        executor_state.stats.max_inventory_gap_abs = Exposure(6.0);
        executor_state.stats.max_gap_age_ms = 120_000;

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        let executor_state = &grid.executor_state;
        assert_eq!(executor_state.slots.len(), 1);
        assert_eq!(executor_state.stats.started_at, resumed_at);
        assert_eq!(executor_state.stats.max_inventory_gap_abs, Exposure(2.0));
        assert_eq!(executor_state.stats.max_gap_age_ms, 0);
        assert_eq!(executor_state.gap_started_at, Some(resumed_at));
    }

    #[test]
    fn resume_grid_does_not_stage_submit_pending_without_emitting_effects() {
        let resumed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(resumed_at)));
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.current_exposure = Exposure(0.0);
        grid.reference_price = Some(95.0);

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.executor_state.slots, vec![empty_inventory_core_slot()]);
        assert_eq!(grid.executor_state.last_reprice_at, None);

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(matches!(
            transition.effects.as_slice(),
            [GridEffect::SubmitOrder { .. }]
        ));
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .slots
                .first()
                .map(|slot| slot.state.clone()),
            Some(SlotState::SubmitPending)
        );
    }

    #[test]
    fn record_submit_receipt_updates_inventory_core_slot() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 95.0,
            quantity: 0.4,
        };
        let receipt = OrderReceipt {
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            status: OrderStatus::New,
        };

        manager
            .record_submit_request(
                &GridId::new("btc1"),
                &request,
                grid_core::types::Exposure(4.0),
            )
            .unwrap();
        manager
            .record_submit_receipt(
                &GridId::new("btc1"),
                &request,
                grid_core::types::Exposure(4.0),
                &receipt,
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            inventory_core_order(grid),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                95.0,
                0.4,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn record_submit_receipt_rejects_receipt_without_matching_executor_slot() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let error = manager
            .record_submit_receipt(
                &GridId::new("btc1"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                },
                grid_core::types::Exposure(4.0),
                &OrderReceipt {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    status: OrderStatus::New,
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("submit receipt"));
    }

    #[test]
    fn record_submit_receipt_accepts_matching_receipt_even_when_state_is_unchanged() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        seed_executor_slot(
            manager.grids.get_mut(&GridId::new("btc1")).unwrap(),
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                95.0,
                0.4,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        manager
            .record_submit_receipt(
                &GridId::new("btc1"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                },
                grid_core::types::Exposure(4.0),
                &OrderReceipt {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    status: OrderStatus::New,
                },
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            inventory_core_order(grid),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                95.0,
                0.4,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn record_submit_failure_clears_submit_pending_slot_by_client_order_id() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
        };
        seed_executor_slot(
            manager.grids.get_mut(&GridId::new("btc1")).unwrap(),
            working_order_from_submit_request(&request, grid_core::types::Exposure(4.0)),
            SlotState::SubmitPending,
        );

        manager
            .record_submit_failure(&GridId::new("btc1"), &request.client_order_id)
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert!(inventory_core_order(grid).is_none());
    }

    #[test]
    fn recover_submit_effect_supersedes_without_receipt_evidence_when_target_is_reached() {
        let mut manager = test_manager_with_cached_price(92.5);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(6.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));

        let recovery = manager
            .recover_submit_effect(
                &GridId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: grid_core::types::Side::Buy,
                    price: 92.5,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                },
                grid_core::types::Exposure(6.0),
                None,
            )
            .unwrap();

        assert!(matches!(
            recovery.resolution,
            executor::SubmitRecoveryResolution::Superseded { .. }
        ));
        assert!(recovery.effects.is_empty());
        assert!(inventory_core_order(manager.get_grid("btc-core").unwrap()).is_none());
    }

    #[test]
    fn recover_submit_effect_supersede_plan_is_executor_owned() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        seed_executor_slot(
            grid,
            working_order(
                None,
                "btc-core-reconcile",
                grid_core::types::Side::Buy,
                94.0,
                test_config().base_qty_per_unit() * 6.0,
                grid_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &GridId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                },
                grid_core::types::Exposure(6.0),
                None,
            )
            .unwrap();

        let executor::SubmitRecoveryResolution::Superseded { state } = &recovery.resolution else {
            panic!("expected stale submit effect to be superseded");
        };
        assert!(matches!(
            recovery.effects.as_slice(),
            [GridEffect::SubmitOrder {
                request,
                target_exposure,
            }] if request.side == grid_core::types::Side::Buy
                && rounded_values_match(request.price, 95.0, test_exchange_rules().price_tick)
                && rounded_values_match(
                    request.quantity,
                    test_config().base_qty_per_unit() * 4.0,
                    test_exchange_rules().quantity_step,
                )
                && *target_exposure == grid_core::types::Exposure(4.0)
        ));
        let replacement_pending = match recovery.effects.as_slice() {
            [
                GridEffect::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => Some(working_order_from_submit_request(
                request,
                target_exposure.clone(),
            )),
            _ => None,
        };
        assert_eq!(
            state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: replacement_pending.clone(),
            }]
        );
        assert_eq!(
            inventory_core_order(manager.get_grid("btc-core").unwrap()),
            replacement_pending.as_ref()
        );
    }

    #[test]
    fn recover_submit_effect_proceeds_when_current_plan_keeps_same_rounded_order_request() {
        let mut manager = test_manager_with_cached_price(94.99);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.config.notional_per_unit = 100.0;
        grid.exchange_rules = grid_core::types::ExchangeRules {
            price_tick: 10.0,
            quantity_step: 1.0,
            min_qty: 0.0,
            min_notional: 0.0,
        };
        let expected_target = grid_core::strategy::target_exposure(94.99, &grid.config);
        grid.target_exposure = Some(expected_target.clone());
        seed_executor_slot(
            grid,
            working_order(
                None,
                "btc-core-reconcile",
                grid_core::types::Side::Buy,
                90.0,
                4.0,
                grid_core::types::Exposure(4.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &GridId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: grid_core::types::Side::Buy,
                    price: 90.0,
                    quantity: 4.0,
                },
                grid_core::types::Exposure(4.0),
                None,
            )
            .unwrap();

        assert!(matches!(
            recovery.resolution,
            executor::SubmitRecoveryResolution::Proceed { .. }
        ));
        assert!(recovery.effects.is_empty());
        assert!(matches!(
            inventory_core_order(manager.get_grid("btc-core").unwrap()),
            Some(WorkingOrder {
                order_id: None,
                client_order_id,
                side: grid_core::types::Side::Buy,
                price,
                quantity,
                target_exposure,
                status: OrderStatus::Submitting,
                role: _,
            }) if client_order_id == "btc-core-reconcile"
                && (*price - 90.0).abs() < f64::EPSILON
                && (*quantity - 4.0).abs() < f64::EPSILON
                && *target_exposure == expected_target
        ));
    }

    #[test]
    fn observe_position_converts_qty_to_exposure_and_updates_unrealized_pnl() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Position(PositionObservation {
                    qty: 15.0,
                    unrealized_pnl: 12.5,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure, grid_core::types::Exposure(4.0));
        assert!((grid.risk_state.unrealized_pnl - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_position_with_cached_reference_price_reconciles_immediately() {
        let mut manager = test_manager_with_cached_price(95.0);

        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                GridObservation::Position(PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 11.0,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.current_exposure,
            grid_core::types::Exposure(2.0)
        );
        assert_eq!(
            transition
                .snapshot
                .target_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
        assert!((transition.snapshot.risk.unrealized_pnl - 11.0).abs() < f64::EPSILON);
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, GridEffect::SubmitOrder { .. }))
        );
    }

    #[test]
    fn sync_exchange_state_clears_stale_inventory_core_slot_when_pending_submit_effect_is_not_preserved()
     {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        seed_executor_slot(
            manager.grids.get_mut(&GridId::new("btc1")).unwrap(),
            working_order(
                Some("stale-1"),
                "stale-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 15.0,
                    unrealized_pnl: 12.5,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure, grid_core::types::Exposure(4.0));
        assert!(inventory_core_order(grid).is_none());
    }

    #[test]
    fn sync_exchange_state_preserves_submit_pending_slot_before_replaying_open_orders() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        seed_executor_slot(
            grid,
            working_order(
                None,
                "restore-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "restore-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure, grid_core::types::Exposure(2.0));
        assert_eq!(
            inventory_core_order(grid),
            Some(&working_order(
                Some("live-1"),
                "restore-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn sync_exchange_state_keeps_paused_grid_target_none() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Paused;
        grid.target_exposure = None;
        grid.reference_price = Some(95.0);

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Paused);
        assert_eq!(grid.target_exposure, None);
        assert_eq!(grid.reference_price, Some(95.0));
    }

    #[test]
    fn sync_exchange_state_marks_attention_required_when_receipt_backed_order_is_missing() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.reference_price = Some(95.0);
        seed_executor_slot(
            grid,
            working_order(
                Some("restore-1"),
                "restore-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![executor::PendingSubmitHint {
                    request: OrderRequest {
                        instrument: test_instrument("BTCUSDT"),
                        client_order_id: "restore-1".into(),
                        side: grid_core::types::Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                    },
                    target_exposure: grid_core::types::Exposure(6.0),
                }],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert_eq!(transition.effects, vec![GridEffect::NoOp]);

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.target_exposure, Some(grid_core::types::Exposure(6.0)));
        assert!(inventory_core_order(grid).is_none());
        assert_eq!(
            grid.executor_state.recovery_anomaly.as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
    }

    #[test]
    fn sync_exchange_state_ignores_pending_submit_effect_without_matching_executor_slot() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.executor_state.slots, vec![empty_inventory_core_slot()]);
        assert!(inventory_core_order(grid).is_none());
    }

    #[test]
    fn sync_exchange_state_replays_live_open_order_without_changing_realized_pnl() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
        };

        manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "live-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }],
                vec![],
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((grid.risk_state.realized_pnl_today - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sync_exchange_state_rebuilds_multiple_live_open_orders_when_they_match_distinct_slots() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        seed_executor_slot(
            grid,
            working_order(
                Some("order-a"),
                "client-a",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        seed_named_executor_slot(
            grid,
            "inventory_followup",
            working_order(
                Some("order-b"),
                "client-b",
                grid_core::types::Side::Sell,
                95.5,
                0.15,
                grid_core::types::Exposure(2.0),
                OrderStatus::PartiallyFilled,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![
                    OrderObservation {
                        order_id: "order-b".into(),
                        client_order_id: "client-b".into(),
                        side: grid_core::types::Side::Sell,
                        price: 95.5,
                        quantity: 0.15,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    OrderObservation {
                        order_id: "order-a".into(),
                        client_order_id: "client-a".into(),
                        side: grid_core::types::Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());
        let grid = manager.get_grid("btc1").unwrap();
        assert!(grid.executor_state.recovery_anomaly.is_none());
        assert_eq!(grid.executor_state.slots.len(), 2);
        assert_eq!(
            grid.executor_state.slots[0].slot,
            OrderSlot::new("inventory_core")
        );
        assert_eq!(
            grid.executor_state.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-a")
        );
        assert_eq!(
            grid.executor_state.slots[1].slot,
            OrderSlot::new("inventory_followup")
        );
        assert_eq!(
            grid.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-b")
        );
    }

    #[test]
    fn observe_order_promotes_matching_pending_slot_for_open_status() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
        };
        manager
            .record_submit_request(
                &GridId::new("btc1"),
                &request,
                grid_core::types::Exposure(6.0),
            )
            .unwrap();

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            inventory_core_order(grid),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(6.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn observe_order_does_not_mutate_slots_while_recovery_anomaly_is_active() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.executor_state = ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: grid_core::types::Exposure(6.0),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![empty_inventory_core_slot()],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: grid_core::types::Exposure(6.0),
                max_gap_age_ms: 0,
            },
        };

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert!(inventory_core_order(grid).is_none());
        assert_eq!(
            grid.executor_state.recovery_anomaly.as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert_eq!(grid.executor_state.slots, vec![empty_inventory_core_slot()]);
    }

    #[test]
    fn canceled_order_keeps_attention_required_while_recovery_anomaly_is_active() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        grid.executor_state = ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: grid_core::types::Exposure(4.0),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![empty_inventory_core_slot()],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: grid_core::types::Exposure(4.0),
                max_gap_age_ms: 0,
            },
        };

        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
        assert_eq!(
            transition.snapshot.executor_state.recovery_anomaly.as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(inventory_core_order_from_snapshot(&transition.snapshot).is_none());
    }

    #[test]
    fn observe_market_updates_gap_stats_when_execution_is_suppressed() {
        let observed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(observed_at)));
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = Exposure(2.0);
        grid.target_exposure = Some(Exposure(4.0));
        grid.executor_state.inventory_gap = Exposure(2.0);
        grid.executor_state.gap_started_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 10, 0, 0).unwrap());
        grid.executor_state.stats.started_at = Utc.with_ymd_and_hms(2026, 3, 29, 9, 45, 0).unwrap();

        let transition = manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Market(MarketObservation {
                    reference_price: 85.0,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
        assert_eq!(transition.snapshot.status, GridStatus::Frozen);
        assert_eq!(
            transition.snapshot.executor_state.inventory_gap,
            Exposure(2.0)
        );
        assert_eq!(
            transition.snapshot.executor_state.gap_started_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 10, 0, 0).unwrap())
        );
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .stats
                .max_inventory_gap_abs,
            Exposure(2.0)
        );
        assert_eq!(
            transition.snapshot.executor_state.stats.max_gap_age_ms,
            30 * 60 * 1000
        );
    }

    #[test]
    fn observe_canceled_order_with_cached_reference_price_reconciles_immediately() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            )
            .unwrap();

        let (request, target_exposure) = match transition.effects.as_slice() {
            [
                GridEffect::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => (request, target_exposure),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order_from_submit_request(
                request,
                target_exposure.clone(),
            ))
        );
    }

    #[test]
    fn observe_filled_order_does_not_reconcile_before_position_update() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &GridId::new("btc-core"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -12.5,
                    status: OrderStatus::Filled,
                }),
            )
            .unwrap();

        assert!(transition.effects.is_empty());
        assert!(inventory_core_order_from_snapshot(&transition.snapshot).is_none());
        assert!((transition.snapshot.risk.realized_pnl_today + 12.5).abs() < f64::EPSILON);
        assert_eq!(
            transition
                .snapshot
                .target_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
    }

    #[test]
    fn observe_order_clears_matching_inventory_core_slot_on_terminal_status() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        seed_executor_slot(
            grid,
            working_order(
                Some("order-1"),
                "client-1",
                grid_core::types::Side::Buy,
                94.5,
                0.25,
                grid_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        grid.executor_state.slots.push(ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::Working,
            working_order: Some(working_order(
                Some("order-2"),
                "client-2",
                grid_core::types::Side::Sell,
                95.5,
                0.15,
                grid_core::types::Exposure(2.0),
                OrderStatus::PartiallyFilled,
            )),
        });

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert!(inventory_core_order(grid).is_none());
        assert_eq!(grid.executor_state.slots.len(), 2);
        assert_eq!(
            grid.executor_state.slots[0].slot,
            OrderSlot::new("inventory_core")
        );
        assert_eq!(grid.executor_state.slots[0].state, SlotState::Empty);
        assert!(grid.executor_state.slots[0].working_order.is_none());
        assert_eq!(
            grid.executor_state.slots[1].slot,
            OrderSlot::new("inventory_followup")
        );
        assert_eq!(
            grid.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-2")
        );
    }

    #[test]
    fn observe_order_accumulates_realized_pnl_by_utc_day() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -12.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((grid.risk_state.realized_pnl_today + 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_order_resets_realized_pnl_when_utc_day_changes() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
        };

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((grid.risk_state.realized_pnl_today + 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_order_keeps_cumulative_realized_pnl_when_utc_day_changes() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
        };

        manager
            .observe(
                &GridId::new("btc1"),
                GridObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((grid.risk_state.realized_pnl_today + 5.0).abs() < f64::EPSILON);
        assert!((grid.risk_state.realized_pnl_cumulative - 15.0).abs() < f64::EPSILON);
    }
}
