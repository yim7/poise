use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::ExchangeRules;
use grid_core::types::Exposure;

use crate::command::GridCommand;
use crate::executor::{self, ExecutionMode, OrderRole, OrderSlot};
use crate::grid::{GridId, Instrument};
use crate::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use crate::ports::{ClockPort, ExchangeOrder, OrderReceipt, OrderRequest};
use crate::reconciler;
use crate::runtime::{
    ExecutionSlot, ExecutionStats, ExecutorState, GridRuntime, GridStatus, PendingOrder, SlotState,
    SubmitRecoveryAnchor, WorkingOrder,
};
use crate::snapshot::GridRuntimeSnapshot;
use crate::transition::{GridEffect, GridTransition};

pub struct GridManager {
    grids: HashMap<GridId, GridRuntime>,
    instruments: HashMap<Instrument, GridId>,
    clock: Arc<dyn ClockPort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitRecoveryResolution {
    Proceed,
    AwaitExchangeState,
    Succeeded,
    Superseded,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitRecoveryPlan {
    pub resolution: SubmitRecoveryResolution,
    pub effects: Vec<GridEffect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitRecoveryAction {
    Proceed,
    AwaitExchangeState,
    RestoreLiveOrder,
    CompleteReceiptBacked,
    Supersede,
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
        submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
    ) -> Result<GridTransition> {
        let (events, effects) =
            self.apply_startup_exchange_state(id, position, open_orders, submit_recovery_anchor)?;
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
        let resumed_state = {
            let grid = self
                .grids
                .get(&GridId::from(id))
                .ok_or_else(|| anyhow::anyhow!("grid `{id}` not found"))?;

            if !matches!(grid.status, GridStatus::Paused) {
                bail!("cannot resume grid `{id}` from status {:?}", grid.status);
            }

            match grid.reference_price {
                Some(reference_price) => {
                    let mut resumed = grid.clone();
                    resumed.status = GridStatus::WaitingMarketData;
                    let result =
                        self.plan_inventory_execution_for_grid(&resumed, reference_price)?;
                    Some((
                        result.new_status.unwrap_or(GridStatus::Active),
                        result.target_exposure,
                        result.replacement_gate_reason,
                    ))
                }
                None => None,
            }
        };

        let grid = self
            .grids
            .get_mut(&GridId::from(id))
            .ok_or_else(|| anyhow::anyhow!("grid `{id}` not found"))?;
        match resumed_state {
            Some((status, exposure, replacement_gate_reason)) => {
                grid.status = status;
                grid.target_exposure = Some(exposure);
                grid.replacement_gate_reason = replacement_gate_reason;
            }
            None => {
                grid.status = GridStatus::WaitingMarketData;
                grid.target_exposure = None;
                grid.replacement_gate_reason = None;
            }
        }

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
        upsert_inventory_core_slot(
            grid,
            self.clock.now(),
            WorkingOrder {
                order_id: None,
                client_order_id: request.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                target_exposure,
                status: crate::ports::OrderStatus::Submitting,
                role: match request.side {
                    grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                    grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
                },
            },
            SlotState::SubmitPending,
        );
        grid.replacement_gate_reason = None;
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
        upsert_inventory_core_slot(
            grid,
            self.clock.now(),
            WorkingOrder {
                order_id: Some(receipt.order_id.clone()),
                client_order_id: receipt.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                target_exposure,
                status: receipt.status,
                role: match request.side {
                    grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                    grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
                },
            },
            SlotState::Working,
        );
        grid.replacement_gate_reason = None;
        Ok(())
    }

    pub fn restore_live_open_order(
        &mut self,
        id: &GridId,
        order: &ExchangeOrder,
        target_exposure: grid_core::types::Exposure,
    ) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        if order.status.keeps_pending_order() {
            upsert_inventory_core_slot(
                grid,
                self.clock.now(),
                WorkingOrder {
                    order_id: Some(order.order_id.clone()),
                    client_order_id: order.client_order_id.clone(),
                    side: order.side,
                    price: order.price,
                    quantity: order.qty,
                    target_exposure,
                    status: order.status,
                    role: match order.side {
                        grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                        grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
                    },
                },
                SlotState::Working,
            );
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn clear_pending_submit(&mut self, id: &GridId, client_order_id: &str) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let cleared_slot = clear_executor_slot(grid, client_order_id, None);
        let cleared_legacy_pending = grid
            .pending_order
            .as_ref()
            .map(|pending| pending.client_order_id == client_order_id)
            .unwrap_or(false);
        if cleared_legacy_pending {
            grid.pending_order = None;
        }
        if cleared_slot || cleared_legacy_pending {
            grid.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn recover_submit_effect(
        &mut self,
        id: &GridId,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryPlan> {
        match self.classify_submit_recovery_effect(
            id,
            request,
            target_exposure.clone(),
            live_order.is_some(),
        )? {
            SubmitRecoveryAction::Proceed => Ok(SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Proceed,
                effects: {
                    self.record_submit_request(id, request, target_exposure)?;
                    vec![]
                },
            }),
            SubmitRecoveryAction::AwaitExchangeState => Ok(SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::AwaitExchangeState,
                effects: vec![],
            }),
            SubmitRecoveryAction::RestoreLiveOrder => {
                let order = live_order.expect("live order must exist for restore");
                self.restore_live_open_order(id, order, target_exposure)?;
                Ok(SubmitRecoveryPlan {
                    resolution: SubmitRecoveryResolution::Succeeded,
                    effects: vec![],
                })
            }
            SubmitRecoveryAction::CompleteReceiptBacked => {
                self.clear_pending_submit(id, &request.client_order_id)?;
                Ok(SubmitRecoveryPlan {
                    resolution: SubmitRecoveryResolution::Succeeded,
                    effects: vec![],
                })
            }
            SubmitRecoveryAction::Supersede => Ok(SubmitRecoveryPlan {
                resolution: SubmitRecoveryResolution::Superseded,
                effects: self.supersede_submit_effect(id, &request.client_order_id)?,
            }),
        }
    }

    fn submit_effect_matches_current_plan(
        &self,
        id: &GridId,
        request: &OrderRequest,
        _target_exposure: grid_core::types::Exposure,
    ) -> Result<bool> {
        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let Some(reference_price) = grid.reference_price else {
            return Ok(false);
        };

        if matches!(grid.status, GridStatus::Paused) {
            return Ok(false);
        }

        let mut planned_grid = grid.clone();
        planned_grid.pending_order = None;
        planned_grid.executor_state = None;
        let result = self.plan_inventory_execution_for_grid(&planned_grid, reference_price)?;

        Ok(matches!(
            result.effects.as_slice(),
            [GridEffect::SubmitOrder {
                request: planned_request,
                ..
            }] if order_requests_match(planned_request, request, &planned_grid.exchange_rules)
        ))
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

        if grid
            .executor_state
            .as_ref()
            .and_then(|state| state.recovery_anomaly.as_ref())
            .is_some()
        {
            return Ok(());
        }

        if observation.status.keeps_pending_order() {
            let target_exposure = Self::resolve_pending_target_exposure(grid);
            upsert_inventory_core_slot(
                grid,
                self.clock.now(),
                WorkingOrder {
                    order_id: Some(observation.order_id.clone()),
                    client_order_id: observation.client_order_id.clone(),
                    side: observation.side,
                    price: observation.price,
                    quantity: observation.quantity,
                    target_exposure,
                    status: observation.status,
                    role: match observation.side {
                        grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                        grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
                    },
                },
                SlotState::Working,
            );
            grid.replacement_gate_reason = None;
            return Ok(());
        }

        if observation.status.clears_pending_order() {
            let cleared_slot = clear_executor_slot(
                grid,
                &observation.client_order_id,
                Some(&observation.order_id),
            );
            let cleared_legacy_pending = grid
                .pending_order
                .as_ref()
                .map(|pending| {
                    pending.client_order_id == observation.client_order_id
                        || pending.order_id.as_deref() == Some(observation.order_id.as_str())
                })
                .unwrap_or(false);
            if cleared_legacy_pending {
                grid.pending_order = None;
            }
            if cleared_slot || cleared_legacy_pending {
                grid.replacement_gate_reason = None;
            }
        }

        Ok(())
    }

    fn apply_startup_exchange_state(
        &mut self,
        id: &GridId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
    ) -> Result<(Vec<DomainEvent>, Vec<GridEffect>)> {
        self.observe_position(id, position)?;
        let observed_at = self.clock.now();
        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?
            .clone();
        let previous_state = grid.executor_state.as_ref().cloned();
        let recovery = executor::recover_working_orders(executor::RecoveryInput {
            exchange_rules: &grid.exchange_rules,
            base_qty_per_unit: grid.config.base_qty_per_unit(),
            current_exposure: &grid.current_exposure,
            target_exposure: grid.target_exposure.as_ref(),
            reference_price: grid.reference_price,
            previous_state: previous_state.as_ref(),
            live_orders: &open_orders,
            submit_recovery_anchor: submit_recovery_anchor.as_ref(),
            observed_at,
        });

        match recovery {
            executor::RecoveryResolution::Anomaly(anomaly) => {
                let grid = self
                    .grids
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                let mut state = previous_state.unwrap_or_else(|| {
                    empty_executor_state(
                        observed_at,
                        grid.current_exposure.clone(),
                        grid.target_exposure
                            .clone()
                            .unwrap_or_else(|| grid.current_exposure.clone()),
                    )
                });
                state.slots.clear();
                state.recovery_anomaly = Some(anomaly);
                grid.executor_state = Some(state);
                sync_pending_order_from_executor_state(grid);
                grid.replacement_gate_reason = None;
                Ok((vec![], vec![GridEffect::NoOp]))
            }
            executor::RecoveryResolution::Rebuilt { state } => {
                let mut planned_grid = grid.clone();
                planned_grid.executor_state = Some(state);
                sync_pending_order_from_executor_state(&mut planned_grid);

                if matches!(planned_grid.status, GridStatus::Paused) {
                    let grid = self
                        .grids
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                    grid.executor_state = planned_grid.executor_state;
                    sync_pending_order_from_executor_state(grid);
                    grid.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                }

                let Some(reference_price) = planned_grid.reference_price else {
                    let grid = self
                        .grids
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                    grid.executor_state = planned_grid.executor_state;
                    sync_pending_order_from_executor_state(grid);
                    grid.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                };

                if submit_recovery_anchor.is_some() {
                    let target = reconciler::reconcile_target(&planned_grid, reference_price);
                    let grid = self
                        .grids
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
                    if let Some(new_status) = target.new_status {
                        grid.status = new_status;
                    }
                    grid.target_exposure = Some(target.target_exposure);
                    grid.reference_price = Some(reference_price);
                    grid.replacement_gate_reason = None;
                    grid.executor_state = planned_grid.executor_state;
                    sync_pending_order_from_executor_state(grid);
                    return Ok((target.events, vec![]));
                }

                let planned =
                    self.plan_inventory_execution_for_grid(&planned_grid, reference_price)?;
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
                grid.executor_state = Some(planned.executor_state);
                sync_pending_order_from_executor_state(grid);
                Ok((planned.events, planned.effects))
            }
        }
    }

    fn resolve_pending_target_exposure(grid: &GridRuntime) -> grid_core::types::Exposure {
        grid.executor_state
            .as_ref()
            .and_then(|state| state.slots.first())
            .and_then(|slot| slot.working_order.as_ref())
            .map(|order| order.target_exposure.clone())
            .or_else(|| grid.target_exposure.clone())
            .unwrap_or_else(|| grid.current_exposure.clone())
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

        let suppress_effects_during_submit_recovery = self.grids[&id]
            .executor_state
            .as_ref()
            .and_then(SubmitRecoveryAnchor::from_executor_state)
            .map(|anchor| anchor.kind == crate::runtime::SubmitRecoveryKind::Submitting)
            .unwrap_or(false);

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
        let effects = if suppress_effects_during_submit_recovery {
            vec![GridEffect::NoOp]
        } else {
            planned_effects
        };

        let grid = self.grids.get_mut(id).unwrap();
        let replacement_gate_event = (grid.replacement_gate_reason != replacement_gate_reason)
            .then(|| replacement_gate_reason.clone())
            .flatten()
            .map(|reason| DomainEvent::PendingOrderKept { reason });
        if let Some(new_status) = new_status {
            grid.status = new_status;
        }
        grid.target_exposure = Some(target_exposure);
        grid.reference_price = Some(reference_price);
        grid.replacement_gate_reason = replacement_gate_reason;
        grid.executor_state = Some(executor_state);
        sync_pending_order_from_executor_state(grid);

        if let Some(event) = replacement_gate_event {
            events.push(event);
        }

        Ok((events, effects))
    }

    fn supersede_submit_effect(
        &mut self,
        id: &GridId,
        client_order_id: &str,
    ) -> Result<Vec<GridEffect>> {
        self.clear_pending_submit(id, client_order_id)?;
        let effects = self.plan_effects_for_current_state(id)?;
        if let [
            GridEffect::SubmitOrder {
                request,
                target_exposure,
            },
        ] = effects.as_slice()
        {
            self.record_submit_request(id, request, target_exposure.clone())?;
        }
        Ok(effects)
    }

    fn plan_effects_for_current_state(&self, id: &GridId) -> Result<Vec<GridEffect>> {
        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let Some(reference_price) = grid.reference_price else {
            return Ok(vec![]);
        };
        if matches!(grid.status, GridStatus::Paused) {
            return Ok(vec![]);
        }

        Ok(self
            .plan_inventory_execution_for_grid(grid, reference_price)?
            .effects
            .into_iter()
            .filter(|effect| !matches!(effect, GridEffect::NoOp))
            .collect())
    }

    fn plan_inventory_execution_for_grid(
        &self,
        grid: &GridRuntime,
        reference_price: f64,
    ) -> Result<PlannedInventoryExecution> {
        let target = reconciler::reconcile_target(grid, reference_price);
        if let Some(executor_state) = grid
            .executor_state
            .as_ref()
            .filter(|state| state.recovery_anomaly.is_some())
        {
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![GridEffect::NoOp],
                target_exposure: target.target_exposure,
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state: executor_state.clone(),
            });
        }
        let observed_at = self.clock.now();
        if target.suppress_execution {
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![GridEffect::NoOp],
                target_exposure: target.target_exposure.clone(),
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state: grid
                    .executor_state
                    .clone()
                    .unwrap_or_else(|| ExecutorState {
                        mode: ExecutionMode::Passive,
                        inventory_gap: grid.current_exposure.delta(&target.target_exposure),
                        gap_started_at: None,
                        last_reprice_at: None,
                        slots: vec![],
                        last_execution_reason: None,
                        recovery_anomaly: None,
                        stats: ExecutionStats {
                            started_at: observed_at,
                            max_inventory_gap_abs: Exposure(0.0),
                            max_gap_age_ms: 0,
                        },
                    }),
            });
        }
        let executor_state = grid.executor_state.as_ref();
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

    fn classify_submit_recovery_effect(
        &self,
        id: &GridId,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        has_live_order: bool,
    ) -> Result<SubmitRecoveryAction> {
        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let receipt_backed = grid
            .executor_state
            .as_ref()
            .and_then(|state| {
                state
                    .slots
                    .iter()
                    .filter_map(|slot| slot.working_order.as_ref())
                    .find(|order| order.client_order_id == request.client_order_id)
            })
            .and_then(|order| order.order_id.as_ref())
            .is_some();
        let still_targeting_effect = grid
            .target_exposure
            .as_ref()
            .map(|current_target| exposures_match(current_target, &target_exposure))
            .unwrap_or(false);
        let target_reached =
            PendingOrder::target_reached(grid.current_exposure.clone(), target_exposure.clone());

        if receipt_backed {
            if has_live_order {
                return Ok(SubmitRecoveryAction::RestoreLiveOrder);
            }

            if target_reached {
                return Ok(SubmitRecoveryAction::CompleteReceiptBacked);
            }

            return Ok(SubmitRecoveryAction::AwaitExchangeState);
        }

        if still_targeting_effect && target_reached {
            return Ok(SubmitRecoveryAction::Supersede);
        }

        if !self.submit_effect_matches_current_plan(id, request, target_exposure)? {
            return Ok(SubmitRecoveryAction::Supersede);
        }

        Ok(SubmitRecoveryAction::Proceed)
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

fn empty_executor_state(
    observed_at: chrono::DateTime<chrono::Utc>,
    current_exposure: Exposure,
    target_exposure: Exposure,
) -> ExecutorState {
    ExecutorState {
        mode: ExecutionMode::Passive,
        inventory_gap: current_exposure.delta(&target_exposure),
        gap_started_at: None,
        last_reprice_at: None,
        slots: vec![],
        last_execution_reason: None,
        recovery_anomaly: None,
        stats: ExecutionStats {
            started_at: observed_at,
            max_inventory_gap_abs: Exposure(0.0),
            max_gap_age_ms: 0,
        },
    }
}

fn upsert_inventory_core_slot(
    grid: &mut GridRuntime,
    observed_at: chrono::DateTime<chrono::Utc>,
    working_order: WorkingOrder,
    state: SlotState,
) {
    let target_exposure = working_order.target_exposure.clone();
    let executor_state = grid.executor_state.get_or_insert_with(|| {
        empty_executor_state(
            observed_at,
            grid.current_exposure.clone(),
            target_exposure.clone(),
        )
    });
    executor_state.slots = vec![ExecutionSlot {
        slot: OrderSlot::new("inventory_core"),
        state,
        working_order: Some(working_order),
    }];
    sync_pending_order_from_executor_state(grid);
}

fn clear_executor_slot(
    grid: &mut GridRuntime,
    client_order_id: &str,
    order_id: Option<&str>,
) -> bool {
    let Some(executor_state) = grid.executor_state.as_mut() else {
        return false;
    };
    let should_clear = executor_state.slots.iter().any(|slot| {
        slot.working_order
            .as_ref()
            .map(|order| {
                order.client_order_id == client_order_id
                    || order_id
                        .map(|order_id| order.order_id.as_deref() == Some(order_id))
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    if should_clear {
        executor_state.slots.clear();
        sync_pending_order_from_executor_state(grid);
    }
    should_clear
}

fn sync_pending_order_from_executor_state(grid: &mut GridRuntime) {
    grid.pending_order = grid
        .executor_state
        .as_ref()
        .and_then(|state| state.slots.first())
        .and_then(|slot| slot.working_order.as_ref())
        .map(|order| PendingOrder {
            order_id: order.order_id.clone(),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.quantity,
            target_exposure: order.target_exposure.clone(),
            status: order.status,
        });
}

fn order_requests_match(
    left: &OrderRequest,
    right: &OrderRequest,
    rules: &grid_core::types::ExchangeRules,
) -> bool {
    left.instrument == right.instrument
        && left.side == right.side
        && left.client_order_id == right.client_order_id
        && rounded_values_match(left.price, right.price, rules.price_tick)
        && rounded_values_match(left.quantity, right.quantity, rules.quantity_step)
}

fn exposures_match(left: &grid_core::types::Exposure, right: &grid_core::types::Exposure) -> bool {
    (left.0 - right.0).abs() <= f64::EPSILON
}

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
        ExecutionSlot, ExecutionStats, ExecutorState, GridStatus, PendingOrder, RiskState,
        SlotState, WorkingOrder,
    };
    use chrono::{TimeZone, Utc};
    use grid_core::events::ReplacementGateReason;
    use grid_core::strategy::*;

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

    fn active_runtime_with_pending_order() -> GridRuntime {
        let mut grid = GridRuntime::new(
            GridId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
        );
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(4.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.pending_order = Some(PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::New,
        });
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
        let pending = grid.pending_order.clone().unwrap();
        seed_executor_slot_from_pending(&mut grid, &pending);
        grid
    }

    fn seed_executor_slot_from_pending(grid: &mut GridRuntime, pending_order: &PendingOrder) {
        upsert_inventory_core_slot(
            grid,
            Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            WorkingOrder {
                order_id: pending_order.order_id.clone(),
                client_order_id: pending_order.client_order_id.clone(),
                side: pending_order.side,
                price: pending_order.price,
                quantity: pending_order.quantity,
                target_exposure: pending_order.target_exposure.clone(),
                status: pending_order.status,
                role: match pending_order.side {
                    grid_core::types::Side::Buy => OrderRole::IncreaseInventory,
                    grid_core::types::Side::Sell => OrderRole::DecreaseInventory,
                },
            },
            if pending_order.is_submit_recovery_anchor() {
                SlotState::SubmitPending
            } else {
                SlotState::Working
            },
        );
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
        let runtime = active_runtime_with_pending_order();
        let snapshot = runtime.snapshot();
        let mut restored = GridRuntime::new(
            GridId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
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
        assert!(
            transition
                .snapshot
                .executor_state
                .as_ref()
                .map(|state| !state.slots.is_empty())
                .unwrap_or(false)
        );
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
        grid.executor_state = Some(passive_executor_state_with_matching_buy_order());

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
        let executor_state = transition.snapshot.executor_state.unwrap();
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
    fn observe_market_suppresses_replan_while_submit_recovery_anchor_exists() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        let pending_order = PendingOrder {
            order_id: None,
            client_order_id: "recover-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::Submitting,
        };
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

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
        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
    }

    #[test]
    fn observe_market_records_submit_recovery_anchor_for_new_submit_effect() {
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
            transition.snapshot.pending_order,
            Some(PendingOrder::from_submit_request(
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
        let pending_order = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Sell,
            price: 99.9,
            quantity: 7.0,
            target_exposure: grid_core::types::Exposure(0.5),
            status: OrderStatus::New,
        };
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

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
            DomainEvent::PendingOrderKept {
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
        let pending_order = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Sell,
            price: 99.9,
            quantity: 7.0,
            target_exposure: grid_core::types::Exposure(0.5),
            status: OrderStatus::New,
        };
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

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
            DomainEvent::PendingOrderKept {
                reason: ReplacementGateReason::RoundedMatch,
            }
        )));
        assert!(
            !second
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::PendingOrderKept { .. }))
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
        let pending_order = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 100.0,
            quantity: 0.1,
            target_exposure: grid_core::types::Exposure(0.4),
            status: OrderStatus::New,
        };
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);
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
            DomainEvent::PendingOrderKept {
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
        let pending_order = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Sell,
            price: 99.9,
            quantity: 7.0,
            target_exposure: grid_core::types::Exposure(0.5),
            status: OrderStatus::New,
        };
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Active);
        assert_eq!(
            grid.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
    }

    #[test]
    fn record_submit_receipt_stores_pending_order() {
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
            .record_submit_receipt(
                &GridId::new("btc1"),
                &request,
                grid_core::types::Exposure(4.0),
                &receipt,
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.pending_order,
            Some(PendingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 95.0,
                quantity: 0.4,
                target_exposure: grid_core::types::Exposure(4.0),
                status: OrderStatus::New,
            })
        );
    }

    #[test]
    fn clear_pending_submit_clears_by_client_order_id() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .pending_order = Some(PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::New,
        });

        manager
            .clear_pending_submit(&GridId::new("btc1"), "client-1")
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.pending_order, None);
    }

    #[test]
    fn recover_submit_effect_supersedes_without_receipt_evidence_when_target_is_reached() {
        let mut manager = test_manager_with_cached_price(92.5);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(6.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.pending_order = None;

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

        assert_eq!(recovery.resolution, SubmitRecoveryResolution::Superseded);
        assert!(recovery.effects.is_empty());
        assert!(
            manager
                .get_grid("btc-core")
                .unwrap()
                .pending_order
                .is_none()
        );
    }

    #[test]
    fn recover_submit_effect_clears_submit_anchor_and_replans_current_target() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.current_exposure = grid_core::types::Exposure(0.0);
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        grid.pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "btc-core-reconcile".into(),
            side: grid_core::types::Side::Buy,
            price: 94.0,
            quantity: test_config().base_qty_per_unit() * 6.0,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::Submitting,
        });

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

        assert_eq!(recovery.resolution, SubmitRecoveryResolution::Superseded);
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
            ] => Some(PendingOrder::from_submit_request(
                request,
                target_exposure.clone(),
            )),
            _ => None,
        };
        assert_eq!(
            manager.get_grid("btc-core").unwrap().pending_order,
            replacement_pending
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
        grid.target_exposure = Some(grid_core::strategy::target_exposure(94.99, &grid.config));
        grid.pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "btc-core-reconcile".into(),
            side: grid_core::types::Side::Buy,
            price: 90.0,
            quantity: 4.0,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::Submitting,
        });

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

        assert_eq!(recovery.resolution, SubmitRecoveryResolution::Proceed);
        assert!(recovery.effects.is_empty());
        assert!(matches!(
            manager.get_grid("btc-core").unwrap().pending_order.as_ref(),
            Some(PendingOrder {
                order_id: None,
                client_order_id,
                side: grid_core::types::Side::Buy,
                price,
                quantity,
                target_exposure,
                status: OrderStatus::Submitting,
            }) if client_order_id == "btc-core-reconcile"
                && (*price - 90.0).abs() < f64::EPSILON
                && (*quantity - 4.0).abs() < f64::EPSILON
                && *target_exposure == grid_core::types::Exposure(4.0)
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
    fn sync_exchange_state_clears_stale_pending_order_when_submit_anchor_is_not_preserved() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .pending_order = Some(PendingOrder {
            order_id: Some("stale-1".into()),
            client_order_id: "stale-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::New,
        });

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 15.0,
                    unrealized_pnl: 12.5,
                },
                vec![],
                None,
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure, grid_core::types::Exposure(4.0));
        assert!(grid.pending_order.is_none());
    }

    #[test]
    fn sync_exchange_state_preserves_submit_anchor_before_replaying_open_orders() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let pending_order = PendingOrder {
            order_id: None,
            client_order_id: "restore-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::Submitting,
        };
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

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
                Some(SubmitRecoveryAnchor {
                    client_order_id: "restore-1".into(),
                    kind: crate::runtime::SubmitRecoveryKind::Submitting,
                }),
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.current_exposure, grid_core::types::Exposure(2.0));
        assert_eq!(
            grid.pending_order,
            Some(PendingOrder {
                order_id: Some("live-1".into()),
                client_order_id: "restore-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: grid_core::types::Exposure(6.0),
                status: OrderStatus::New,
            })
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
                None,
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
    fn sync_exchange_state_preserves_recovery_slot_without_emitting_effects() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let pending_order = PendingOrder {
            order_id: Some("restore-1".into()),
            client_order_id: "restore-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::New,
        };
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.status = GridStatus::Active;
        grid.current_exposure = grid_core::types::Exposure(2.0);
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.reference_price = Some(95.0);
        grid.pending_order = Some(pending_order.clone());
        seed_executor_slot_from_pending(grid, &pending_order);

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                Some(SubmitRecoveryAnchor {
                    client_order_id: "restore-1".into(),
                    kind: crate::runtime::SubmitRecoveryKind::ReceiptBacked,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.events,
            vec![DomainEvent::ExposureTargetChanged {
                from: grid_core::types::Exposure(2.0),
                to: grid_core::types::Exposure(4.0),
            }]
        );
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.target_exposure, Some(grid_core::types::Exposure(4.0)));
        assert_eq!(
            grid.pending_order,
            Some(PendingOrder {
                order_id: Some("restore-1".into()),
                client_order_id: "restore-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: grid_core::types::Exposure(6.0),
                status: OrderStatus::New,
            })
        );
        assert_eq!(
            grid.executor_state
                .as_ref()
                .map(|state| state.slots.clone())
                .unwrap_or_default(),
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("restore-1".into()),
                    client_order_id: "restore-1".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    target_exposure: grid_core::types::Exposure(6.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
    }

    #[test]
    fn sync_exchange_state_does_not_preserve_submit_anchor_from_legacy_pending_order_only_state() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "legacy-submit".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::Submitting,
        });

        let transition = manager
            .sync_exchange_state(
                &GridId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                Some(SubmitRecoveryAnchor {
                    client_order_id: "legacy-submit".into(),
                    kind: crate::runtime::SubmitRecoveryKind::Submitting,
                }),
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let grid = manager.get_grid("btc1").unwrap();
        assert!(
            grid.executor_state
                .as_ref()
                .map(|state| state.slots.is_empty())
                .unwrap_or(true)
        );
        assert!(grid.pending_order.is_none());
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
                None,
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
    fn sync_exchange_state_rejects_multiple_live_open_orders() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

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
                        client_order_id: "b".into(),
                        side: grid_core::types::Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    OrderObservation {
                        order_id: "order-a".into(),
                        client_order_id: "a".into(),
                        side: grid_core::types::Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ],
                None,
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(
            grid.executor_state
                .as_ref()
                .and_then(|state| state.recovery_anomaly.as_ref()),
            Some(&crate::executor::RecoveryAnomaly::DuplicateLiveOrders)
        );
    }

    #[test]
    fn observe_order_rebuilds_pending_order_for_open_status() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .target_exposure = Some(grid_core::types::Exposure(6.0));

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
            grid.pending_order,
            Some(PendingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: grid_core::types::Exposure(6.0),
                status: OrderStatus::New,
            })
        );
    }

    #[test]
    fn observe_order_does_not_mutate_slots_while_recovery_anomaly_is_active() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");
        let grid = manager.grids.get_mut(&GridId::new("btc1")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(6.0));
        grid.executor_state = Some(ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: grid_core::types::Exposure(6.0),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: grid_core::types::Exposure(6.0),
                max_gap_age_ms: 0,
            },
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
                    status: OrderStatus::New,
                }),
            )
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert!(grid.pending_order.is_none());
        assert_eq!(
            grid.executor_state
                .as_ref()
                .and_then(|state| state.recovery_anomaly.as_ref()),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(
            grid.executor_state
                .as_ref()
                .map(|state| state.slots.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn canceled_order_keeps_attention_required_while_recovery_anomaly_is_active() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        grid.executor_state = Some(ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: grid_core::types::Exposure(4.0),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![],
            last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
            recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: grid_core::types::Exposure(4.0),
                max_gap_age_ms: 0,
            },
        });

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
            transition
                .snapshot
                .executor_state
                .as_ref()
                .and_then(|state| state.recovery_anomaly.as_ref()),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(transition.snapshot.pending_order.is_none());
    }

    #[test]
    fn observe_canceled_order_with_cached_reference_price_reconciles_immediately() {
        let mut manager = test_manager_with_cached_price(95.0);
        let grid = manager.grids.get_mut(&GridId::new("btc-core")).unwrap();
        grid.target_exposure = Some(grid_core::types::Exposure(4.0));
        grid.pending_order = Some(PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::New,
        });

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
            transition.snapshot.pending_order,
            Some(PendingOrder::from_submit_request(
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
        grid.pending_order = Some(PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::New,
        });

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
        assert!(transition.snapshot.pending_order.is_none());
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
    fn observe_order_clears_matching_pending_order_on_terminal_status() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .pending_order = Some(PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::New,
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
        assert_eq!(grid.pending_order, None);
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
