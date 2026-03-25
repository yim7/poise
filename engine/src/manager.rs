use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::ExchangeRules;

use crate::command::GridCommand;
use crate::grid::{GridId, Instrument};
use crate::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use crate::ports::{ClockPort, OrderStatus};
use crate::reconciler;
use crate::runtime::{GridRuntime, GridStatus, PendingOrder};
use crate::snapshot::GridRuntimeSnapshot;
use crate::transition::{GridEffect, GridTransition};

pub struct GridManager {
    grids: HashMap<GridId, GridRuntime>,
    budgets: HashMap<GridId, CapacityBudget>,
    instruments: HashMap<Instrument, GridId>,
    clock: Arc<dyn ClockPort>,
}

impl GridManager {
    pub fn new(clock: Arc<dyn ClockPort>) -> Self {
        Self {
            grids: HashMap::new(),
            budgets: HashMap::new(),
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
        let grid = GridRuntime::new(id.clone(), instrument.clone(), config, exchange_rules);
        self.grids.insert(id.clone(), grid);
        self.budgets.insert(id.clone(), budget);
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
                (vec![], vec![])
            }
            GridObservation::Order(observation) => {
                self.observe_order(id, observation)?;
                (vec![], vec![])
            }
        };

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
                    let budget = self
                        .budgets
                        .get(&GridId::from(id))
                        .ok_or_else(|| anyhow::anyhow!("budget for grid `{id}` not found"))?;
                    let mut resumed = grid.clone();
                    resumed.status = GridStatus::WaitingMarketData;
                    let result = reconciler::reconcile(&resumed, reference_price, budget);
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
        let exchange_rules = grid.exchange_rules.clone();
        *grid = GridRuntime::restore(snapshot.clone(), exchange_rules)?;
        Ok(())
    }

    pub fn record_submitted_order(&mut self, id: &GridId, pending: PendingOrder) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        grid.pending_order = Some(pending);
        Ok(())
    }

    pub fn clear_pending_order(&mut self, id: &GridId) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        grid.pending_order = None;
        Ok(())
    }

    pub fn mark_pending_order_canceling(&mut self, id: &GridId) -> Result<()> {
        let grid = self
            .grids
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        if let Some(pending) = &mut grid.pending_order {
            pending.status = OrderStatus::Canceling;
        }
        Ok(())
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
            let target_exposure = grid
                .pending_order
                .as_ref()
                .map(|pending| pending.target_exposure.clone())
                .or_else(|| grid.target_exposure.clone())
                .unwrap_or_else(|| grid.current_exposure.clone());

            grid.pending_order = Some(PendingOrder {
                order_id: Some(observation.order_id),
                client_order_id: observation.client_order_id,
                side: observation.side,
                price: observation.price,
                quantity: observation.quantity,
                target_exposure,
                status: observation.status,
            });
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

        let grid = self
            .grids
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("grid `{}` not found", id.as_str()))?;
        let budget = self
            .budgets
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("budget for grid `{}` not found", id.as_str()))?;
        let result = reconciler::reconcile(grid, reference_price, budget);

        let grid = self.grids.get_mut(id).unwrap();
        if let Some(new_status) = result.new_status {
            grid.status = new_status;
        }
        grid.target_exposure = Some(result.target_exposure);
        grid.reference_price = Some(reference_price);

        Ok((result.events, result.effects))
    }
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
        let restored = GridRuntime::restore(snapshot.clone(), test_exchange_rules()).unwrap();

        assert_eq!(restored.snapshot(), snapshot);
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
        let transition = manager
            .command(
                &GridId::new("btc-core"),
                crate::command::GridCommand::Reconcile,
            )
            .unwrap();

        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
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

        manager.resume_grid("btc1").unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.status, GridStatus::Frozen);
        assert_eq!(grid.current_exposure.0, 8.0);
    }

    #[test]
    fn record_submitted_order_stores_pending_order() {
        let mut manager = test_manager();
        register_test_grid(&mut manager, "btc1", "BTCUSDT");

        let pending = PendingOrder {
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 95.0,
            quantity: 0.4,
            target_exposure: grid_core::types::Exposure(4.0),
            status: OrderStatus::New,
        };

        manager
            .record_submitted_order(&GridId::new("btc1"), pending.clone())
            .unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.pending_order, Some(pending));
    }

    #[test]
    fn clear_pending_order_clears_by_grid_id() {
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

        manager.clear_pending_order(&GridId::new("btc1")).unwrap();

        let grid = manager.get_grid("btc1").unwrap();
        assert_eq!(grid.pending_order, None);
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
