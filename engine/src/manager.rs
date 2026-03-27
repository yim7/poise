use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::ExchangeRules;

use crate::command::GridCommand;
use crate::grid::{GridId, Instrument};
use crate::observation::{GridObservation, MarketObservation, OrderObservation, PositionObservation};
use crate::ports::{ClockPort, ExchangeOrder, OrderReceipt, OrderRequest};
use crate::reconciler;
use crate::runtime::{GridRuntime, GridStatus, PendingOrder, SubmitRecoveryAnchor};
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
        self.apply_startup_exchange_state(id, position, open_orders, submit_recovery_anchor)?;
        self.transition_for(id, vec![], vec![])
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
                    let result = reconciler::reconcile(&resumed, reference_price);
                    Some((
                        result.new_status.unwrap_or(GridStatus::Active),
                        result.target_exposure,
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
            Some((status, exposure)) => {
                grid.status = status;
                grid.target_exposure = Some(exposure);
            }
            None => {
                grid.status = GridStatus::WaitingMarketData;
                grid.target_exposure = None;
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
        grid.pending_order = Some(PendingOrder::from_submit_request(request, target_exposure));
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
        grid.pending_order =
            Some(PendingOrder::from_submit_receipt(request, target_exposure, receipt));
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
            grid.pending_order = Some(PendingOrder::from_exchange_order(order, target_exposure));
        }
        Ok(())
    }

    pub fn clear_pending_submit(&mut self, id: &GridId, client_order_id: &str) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        if grid
            .pending_order
            .as_ref()
            .map(|pending| pending.client_order_id == client_order_id)
            .unwrap_or(false)
        {
            grid.pending_order = None;
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
        target_exposure: grid_core::types::Exposure,
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

        if let Some(pending_order) = grid.pending_order.as_ref() {
            if pending_order.client_order_id != request.client_order_id
                || !pending_order.is_submit_recovery_anchor()
            {
                return Ok(false);
            }
        }

        let mut planned_grid = grid.clone();
        planned_grid.pending_order = None;
        let result = reconciler::reconcile(&planned_grid, reference_price);

        Ok(matches!(
            result.effects.as_slice(),
            [GridEffect::SubmitOrder {
                request: planned_request,
                target_exposure: planned_target,
            }] if order_requests_match(planned_request, request, &planned_grid.exchange_rules)
                && exposures_match(planned_target, &target_exposure)
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
        }

        if observation.status.keeps_pending_order() {
            let target_exposure = Self::resolve_pending_target_exposure(grid);
            grid.pending_order =
                Some(PendingOrder::from_order_observation(&observation, target_exposure));
            return Ok(());
        }

        if observation.status.clears_pending_order() {
            let should_clear = grid
                .pending_order
                .as_ref()
                .map(|pending| {
                    pending.order_id.as_deref() == Some(observation.order_id.as_str())
                        || pending.client_order_id == observation.client_order_id
                })
                .unwrap_or(false);

            if should_clear {
                grid.pending_order = None;
            }
        }

        Ok(())
    }

    fn apply_startup_exchange_state(
        &mut self,
        id: &GridId,
        position: PositionObservation,
        mut open_orders: Vec<OrderObservation>,
        submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
    ) -> Result<()> {
        open_orders.sort_by(|left, right| {
            left.client_order_id
                .cmp(&right.client_order_id)
                .then_with(|| left.order_id.cmp(&right.order_id))
        });
        if open_orders.len() > 1 {
            let open_orders = open_orders
                .iter()
                .map(|order| format!("{}/{}", order.client_order_id, order.order_id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "grid `{}` received multiple live open orders during startup sync: {}",
                id.as_str(),
                open_orders
            );
        }

        self.observe_position(id, position)?;
        let preserve_submit_anchor = self
            .grids
            .get(id)
            .and_then(|grid| {
                let pending = grid.pending_order.as_ref()?;
                submit_recovery_anchor
                    .as_ref()
                    .filter(|anchor| anchor.matches(pending))
            })
            .is_some();
        if !preserve_submit_anchor {
            self.clear_pending_order(id)?;
        }
        if let Some(open_order) = open_orders.into_iter().next() {
            self.replay_live_open_order(id, open_order)?;
        }
        Ok(())
    }

    fn replay_live_open_order(&mut self, id: &GridId, observation: OrderObservation) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;

        if !observation.status.keeps_pending_order() {
            return Ok(());
        }

        let target_exposure = Self::resolve_pending_target_exposure(grid);
        grid.pending_order = Some(PendingOrder::from_order_observation(
            &observation,
            target_exposure,
        ));

        Ok(())
    }

    fn resolve_pending_target_exposure(grid: &GridRuntime) -> grid_core::types::Exposure {
        grid.pending_order
            .as_ref()
            .map(|pending| pending.target_exposure.clone())
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
            return Ok((vec![], vec![]));
        }

        let suppress_effects_during_submit_recovery = self.grids[&id]
            .pending_order
            .as_ref()
            .map(PendingOrder::is_submit_recovery_anchor)
            .unwrap_or(false);

        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let result = reconciler::reconcile(grid, reference_price);
        let effects = if suppress_effects_during_submit_recovery {
            vec![GridEffect::NoOp]
        } else {
            result.effects
        };

        let grid = self.grids.get_mut(id).unwrap();
        if let Some(new_status) = result.new_status {
            grid.status = new_status;
        }
        grid.target_exposure = Some(result.target_exposure);
        grid.reference_price = Some(reference_price);

        Ok((result.events, effects))
    }

    fn clear_pending_order(&mut self, id: &GridId) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        grid.pending_order = None;
        Ok(())
    }

    fn supersede_submit_effect(
        &mut self,
        id: &GridId,
        client_order_id: &str,
    ) -> Result<Vec<GridEffect>> {
        self.clear_pending_submit(id, client_order_id)?;
        self.plan_effects_for_current_state(id)
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

        Ok(reconciler::reconcile(grid, reference_price)
            .effects
            .into_iter()
            .filter(|effect| !matches!(effect, GridEffect::NoOp))
            .collect())
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
        let restored_pending = grid
            .pending_order
            .as_ref()
            .filter(|pending| pending.client_order_id == request.client_order_id)
            .cloned();
        let receipt_backed = restored_pending
            .as_ref()
            .and_then(|pending| pending.order_id.as_ref())
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
    use crate::ports::*;
    use crate::runtime::{GridStatus, PendingOrder, RiskState};
    use chrono::{TimeZone, Utc};
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
            unrealized_pnl: -3.0,
        };
        grid.reference_price = Some(95.0);
        grid.out_of_band_since = Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap());
        grid
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

        let result = crate::reconciler::reconcile(&runtime, 90.0);
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
        grid.pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "recover-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.0,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(6.0),
            status: OrderStatus::Submitting,
        });

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
        assert!(manager.get_grid("btc-core").unwrap().pending_order.is_none());
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
        assert!(manager.get_grid("btc-core").unwrap().pending_order.is_none());
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
        manager
            .grids
            .get_mut(&GridId::new("btc1"))
            .unwrap()
            .pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: "restore-1".into(),
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

        let error = manager
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
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("a/order-a, b/order-b"),
            "unexpected error: {error}"
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

        assert!(transition.snapshot.pending_order.is_none());
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, GridEffect::SubmitOrder { .. }))
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
}
