use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::ExchangeRules;

use crate::execution_plan::ExecutionPlan;
use crate::instance::{InstanceStatus, PendingOrder, StrategyInstance};
use crate::ports::{
    ClockPort, ExchangePort, InstanceSnapshot, OpenOrder, PersistencePort, Position, PriceTick,
};
use crate::reconciler;

pub struct TickOutcome {
    pub plan: ExecutionPlan,
    pub events: Vec<DomainEvent>,
}

pub struct InstanceManager {
    instances: HashMap<String, StrategyInstance>,
    budgets: HashMap<String, CapacityBudget>,
    exchange: Arc<dyn ExchangePort>,
    persistence: Arc<dyn PersistencePort>,
    clock: Arc<dyn ClockPort>,
}

impl InstanceManager {
    pub fn new(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<dyn PersistencePort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            instances: HashMap::new(),
            budgets: HashMap::new(),
            exchange,
            persistence,
            clock,
        }
    }

    pub fn add_instance(
        &mut self,
        id: String,
        symbol: String,
        config: GridConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
    ) -> Result<()> {
        if self.instances.contains_key(&id) {
            bail!("duplicate instance id `{id}`");
        }
        grid_core::strategy::validate_config(&config).map_err(|e| anyhow::anyhow!(e))?;
        let instance = StrategyInstance::new(id.clone(), symbol, config, exchange_rules);
        self.instances.insert(id.clone(), instance);
        self.budgets.insert(id, budget);
        Ok(())
    }

    pub fn on_price_tick(&mut self, tick: &PriceTick) -> Result<TickOutcome> {
        let mut all_events = vec![];
        let mut actions = vec![];
        let mut plan_events = vec![];
        let ids: Vec<String> = self
            .instances
            .keys()
            .filter(|id| self.instances[*id].symbol == tick.symbol)
            .cloned()
            .collect();

        for id in ids {
            if matches!(self.instances[&id].status, InstanceStatus::Paused) {
                let instance = self.instances.get_mut(&id).unwrap();
                instance.last_price = Some(tick.last_price);
                instance.target_exposure = None;
                continue;
            }

            let instance = self.instances.get(&id).unwrap();
            let budget = self.budgets.get(&id).unwrap();
            let result = reconciler::reconcile(instance, tick.last_price, budget);

            let instance = self.instances.get_mut(&id).unwrap();
            if let Some(new_status) = result.new_status {
                instance.status = new_status;
            }
            instance.target_exposure = Some(result.target_exposure);
            instance.last_price = Some(tick.last_price);

            actions.extend(result.plan.actions);
            all_events.extend(result.plan.events.clone());
            plan_events.extend(result.plan.events);
        }

        Ok(TickOutcome {
            plan: ExecutionPlan {
                actions,
                events: plan_events,
            },
            events: all_events,
        })
    }

    pub fn pause_instance(&mut self, id: &str) -> Result<()> {
        let instance = self
            .instances
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("instance `{id}` not found"))?;
        // Pause disables strategy targeting, but does not rewrite observed exchange state.
        instance.status = InstanceStatus::Paused;
        instance.target_exposure = None;
        Ok(())
    }

    pub fn resume_instance(&mut self, id: &str) -> Result<()> {
        let resumed_state = {
            let instance = self
                .instances
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("instance `{id}` not found"))?;

            if !matches!(instance.status, InstanceStatus::Paused) {
                bail!(
                    "cannot resume instance `{id}` from status {:?}",
                    instance.status
                );
            }

            match instance.last_price {
                Some(last_price) => {
                    let budget = self
                        .budgets
                        .get(id)
                        .ok_or_else(|| anyhow::anyhow!("budget for instance `{id}` not found"))?;
                    let mut resumed = instance.clone();
                    resumed.status = InstanceStatus::WaitingMarketData;
                    let result = reconciler::reconcile(&resumed, last_price, budget);
                    Some((
                        result.new_status.unwrap_or(InstanceStatus::Active),
                        result.target_exposure,
                    ))
                }
                None => None,
            }
        };

        let instance = self
            .instances
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("instance `{id}` not found"))?;
        match resumed_state {
            Some((status, exposure)) => {
                instance.status = status;
                instance.target_exposure = Some(exposure);
            }
            None => {
                instance.status = InstanceStatus::WaitingMarketData;
                instance.target_exposure = None;
            }
        }

        Ok(())
    }

    pub fn restore_instance_state(&mut self, snapshot: &InstanceSnapshot) -> Result<()> {
        let instance = self
            .instances
            .get_mut(&snapshot.id)
            .ok_or_else(|| anyhow::anyhow!("instance `{}` not found", snapshot.id))?;
        if instance.symbol != snapshot.symbol {
            bail!(
                "snapshot symbol mismatch for `{}`: expected `{}`, got `{}`",
                snapshot.id,
                instance.symbol,
                snapshot.symbol
            );
        }

        instance.status = snapshot.status.clone();
        instance.current_exposure = snapshot.current_exposure.clone();
        instance.target_exposure = snapshot.target_exposure.clone();
        instance.pending_order = snapshot.pending_order.clone();
        instance.risk_state = snapshot.risk_state.clone();
        instance.last_price = snapshot.last_price;
        instance.out_of_band_since = snapshot.out_of_band_since;
        Ok(())
    }

    pub fn record_submitted_order(&mut self, id: &str, pending: PendingOrder) -> Result<()> {
        let instance = self
            .instances
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("instance `{id}` not found"))?;
        instance.pending_order = Some(pending);
        Ok(())
    }

    pub fn clear_pending_order(&mut self, symbol: &str) -> Result<()> {
        let instance = self.instance_by_symbol_mut(symbol)?;
        instance.pending_order = None;
        Ok(())
    }

    pub fn apply_position_update(&mut self, position: &Position) -> Result<()> {
        let instance = self.instance_by_symbol_mut(&position.symbol)?;
        let unit_qty = instance.config.capacity_unit_qty();
        instance.current_exposure = if unit_qty <= f64::EPSILON {
            grid_core::types::Exposure(0.0)
        } else {
            grid_core::types::Exposure(position.qty / unit_qty)
        };
        instance.risk_state.unrealized_pnl = position.unrealized_pnl;
        Ok(())
    }

    pub fn apply_order_update(&mut self, order: &OpenOrder) -> Result<()> {
        let today = self.clock.now().date_naive();
        let instance = self.instance_by_symbol_mut(&order.symbol)?;

        if instance.risk_state.realized_pnl_day != Some(today) {
            instance.risk_state.realized_pnl_day = Some(today);
            instance.risk_state.realized_pnl_today = 0.0;
        }
        if order.realized_pnl.abs() > f64::EPSILON {
            instance.risk_state.realized_pnl_today += order.realized_pnl;
        }

        if matches!(order.status.as_str(), "NEW" | "PARTIALLY_FILLED") {
            let target_exposure = instance
                .pending_order
                .as_ref()
                .map(|pending| pending.target_exposure.clone())
                .or_else(|| instance.target_exposure.clone())
                .unwrap_or_else(|| instance.current_exposure.clone());

            instance.pending_order = Some(PendingOrder {
                symbol: order.symbol.clone(),
                order_id: Some(order.order_id.clone()),
                client_order_id: order.client_order_id.clone(),
                side: order.side,
                price: order.price,
                quantity: order.qty,
                target_exposure,
                status: order.status.clone(),
            });
            return Ok(());
        }

        if matches!(
            order.status.as_str(),
            "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED"
        ) {
            let should_clear = instance
                .pending_order
                .as_ref()
                .map(|pending| {
                    pending.order_id.as_deref() == Some(order.order_id.as_str())
                        || pending.client_order_id == order.client_order_id
                })
                .unwrap_or(false);

            if should_clear {
                instance.pending_order = None;
            }
        }

        Ok(())
    }

    pub fn list_instances(&self) -> Vec<&StrategyInstance> {
        self.instances.values().collect()
    }

    pub fn get_instance(&self, id: &str) -> Option<&StrategyInstance> {
        self.instances.get(id)
    }

    pub fn exchange(&self) -> &dyn ExchangePort {
        self.exchange.as_ref()
    }

    pub fn persistence(&self) -> &dyn PersistencePort {
        self.persistence.as_ref()
    }

    pub fn clock(&self) -> &dyn ClockPort {
        self.clock.as_ref()
    }

    fn instance_by_symbol_mut(&mut self, symbol: &str) -> Result<&mut StrategyInstance> {
        self.instances
            .values_mut()
            .find(|instance| instance.symbol == symbol)
            .ok_or_else(|| anyhow::anyhow!("instance for symbol `{symbol}` not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::InstanceStatus;
    use crate::ports::*;
    use chrono::{TimeZone, Utc};
    use grid_core::strategy::*;

    // ── Fake adapters for testing ──

    struct FakeExchange;

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            unimplemented!()
        }
        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> Result<()> {
            unimplemented!()
        }
        async fn cancel_all(&self, _symbol: &str) -> Result<()> {
            unimplemented!()
        }
        async fn get_position(&self, _symbol: &str) -> Result<Position> {
            unimplemented!()
        }
        async fn get_open_orders(&self, _symbol: &str) -> Result<Vec<OpenOrder>> {
            unimplemented!()
        }
        async fn get_exchange_info(&self, _symbol: &str) -> Result<ExchangeInfo> {
            unimplemented!()
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl PersistencePort for FakePersistence {
        async fn save_instance_state(&self, _id: &str, _state: &InstanceSnapshot) -> Result<()> {
            Ok(())
        }
        async fn load_instance_state(&self, _id: &str) -> Result<Option<InstanceSnapshot>> {
            Ok(None)
        }
    }

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

    fn test_manager() -> InstanceManager {
        InstanceManager::new(
            Arc::new(FakeExchange),
            Arc::new(FakePersistence),
            Arc::new(FakeClock),
        )
    }

    fn test_manager_with_clock(clock: Arc<dyn ClockPort>) -> InstanceManager {
        InstanceManager::new(Arc::new(FakeExchange), Arc::new(FakePersistence), clock)
    }

    fn test_config() -> GridConfig {
        GridConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_capacity: 8.0,
            short_capacity: 8.0,
            capacity_notional: 375.0,
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

    #[test]
    fn add_instance_validates_config() {
        let mut manager = test_manager();
        let bad_config = GridConfig {
            lower_price: 110.0,
            upper_price: 90.0,
            ..test_config()
        };
        assert!(
            manager
                .add_instance(
                    "test".into(),
                    "BTCUSDT".into(),
                    bad_config,
                    test_budget(),
                    test_exchange_rules(),
                )
                .is_err()
        );
    }

    #[test]
    fn add_and_list_instances() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
        manager
            .add_instance(
                "eth1".into(),
                "ETHUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        assert_eq!(manager.list_instances().len(), 2);
        assert!(manager.get_instance("btc1").is_some());
        assert!(manager.get_instance("eth1").is_some());
        assert!(manager.get_instance("nonexistent").is_none());
    }

    #[test]
    fn add_instance_rejects_duplicate_ids() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "dup".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager
            .add_instance(
                "dup".into(),
                "ETHUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("duplicate instance id"));
    }

    #[test]
    fn on_price_tick_updates_instance() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let outcome = manager.on_price_tick(&tick).unwrap();
        assert!(!outcome.events.is_empty());

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::Active);
        assert_eq!(instance.last_price, Some(95.0));
        assert_eq!(instance.current_exposure.0, 0.0);
        assert!(instance.target_exposure.as_ref().unwrap().0 > 0.0); // should be long below center
    }

    #[test]
    fn on_price_tick_returns_tick_outcome_with_plan_and_events() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let outcome = manager.on_price_tick(&tick).unwrap();

        assert!(!outcome.plan.actions.is_empty());
        assert!(!outcome.events.is_empty());
    }

    #[test]
    fn on_price_tick_updates_target_without_faking_current_exposure() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let instance = manager.instances.get_mut("btc1").unwrap();
        instance.current_exposure = grid_core::types::Exposure(2.0);

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let outcome = manager.on_price_tick(&tick).unwrap();

        assert!(!outcome.events.is_empty());

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.current_exposure.0, 2.0);
        assert_eq!(instance.target_exposure.as_ref().unwrap().0, 4.0);
        assert_eq!(instance.last_price, Some(95.0));
    }

    #[test]
    fn on_price_tick_ignores_unrelated_symbol() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let tick = PriceTick {
            symbol: "ETHUSDT".into(),
            last_price: 2500.0,
            mark_price: 2500.0,
            timestamp: Utc::now(),
        };

        let outcome = manager.on_price_tick(&tick).unwrap();
        assert!(outcome.events.is_empty());
        assert!(outcome.plan.actions.is_empty());

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::WaitingMarketData);
    }

    #[test]
    fn paused_instance_ignores_reconcile_updates() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
        let instance = manager.instances.get_mut("btc1").unwrap();
        instance.status = InstanceStatus::Paused;
        instance.current_exposure = grid_core::types::Exposure(2.0);
        instance.target_exposure = Some(grid_core::types::Exposure(4.0));

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let outcome = manager.on_price_tick(&tick).unwrap();

        assert!(outcome.events.is_empty());
        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::Paused);
        assert_eq!(instance.current_exposure.0, 2.0);
        assert_eq!(instance.target_exposure, None);
        assert_eq!(instance.last_price, Some(95.0));
    }

    #[test]
    fn resume_instance_rejects_non_paused_status() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager.resume_instance("btc1").unwrap_err();

        assert!(error.to_string().contains("cannot resume"));
    }

    #[test]
    fn resume_instance_recomputes_status_from_last_price() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let instance = manager.instances.get_mut("btc1").unwrap();
        instance.status = InstanceStatus::Paused;
        instance.current_exposure = grid_core::types::Exposure(8.0);
        instance.last_price = Some(85.0);

        manager.resume_instance("btc1").unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::Frozen);
        assert_eq!(instance.current_exposure.0, 8.0);
    }

    #[test]
    fn record_submitted_order_stores_pending_order() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let pending = crate::instance::PendingOrder {
            symbol: "BTCUSDT".into(),
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 95.0,
            quantity: 0.4,
            target_exposure: grid_core::types::Exposure(4.0),
            status: "NEW".into(),
        };

        manager
            .record_submitted_order("btc1", pending.clone())
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.pending_order, Some(pending));
    }

    #[test]
    fn clear_pending_order_clears_by_symbol() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
        manager.instances.get_mut("btc1").unwrap().pending_order = Some(PendingOrder {
            symbol: "BTCUSDT".into(),
            order_id: Some("order-1".into()),
            client_order_id: "client-1".into(),
            side: grid_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: grid_core::types::Exposure(4.0),
            status: "NEW".into(),
        });

        manager.clear_pending_order("BTCUSDT").unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.pending_order, None);
    }

    #[test]
    fn apply_position_update_converts_qty_to_exposure_and_updates_unrealized_pnl() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        manager
            .apply_position_update(&Position {
                symbol: "BTCUSDT".into(),
                qty: 15.0,
                avg_price: 100.0,
                unrealized_pnl: 12.5,
            })
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.current_exposure, grid_core::types::Exposure(4.0));
        assert!((instance.risk_state.unrealized_pnl - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_order_update_rebuilds_pending_order_for_open_status() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        manager.instances.get_mut("btc1").unwrap().target_exposure =
            Some(grid_core::types::Exposure(6.0));

        manager
            .apply_order_update(&OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                qty: 0.25,
                realized_pnl: 0.0,
                status: "NEW".into(),
            })
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(
            instance.pending_order,
            Some(crate::instance::PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: grid_core::types::Exposure(6.0),
                status: "NEW".into(),
            })
        );
    }

    #[test]
    fn apply_order_update_clears_matching_pending_order_on_terminal_status() {
        let mut manager = test_manager();
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        manager.instances.get_mut("btc1").unwrap().pending_order =
            Some(crate::instance::PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: grid_core::types::Exposure(4.0),
                status: "NEW".into(),
            });

        manager
            .apply_order_update(&OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                qty: 0.25,
                realized_pnl: 0.0,
                status: "FILLED".into(),
            })
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.pending_order, None);
    }

    #[test]
    fn apply_order_update_accumulates_realized_pnl_by_utc_day() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        manager
            .apply_order_update(&OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                qty: 0.25,
                realized_pnl: -12.5,
                status: "PARTIALLY_FILLED".into(),
            })
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(
            instance.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((instance.risk_state.realized_pnl_today + 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_order_update_resets_realized_pnl_when_utc_day_changes() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        manager
            .add_instance(
                "btc1".into(),
                "BTCUSDT".into(),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
        manager.instances.get_mut("btc1").unwrap().risk_state = crate::instance::RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            unrealized_pnl: 0.0,
        };

        manager
            .apply_order_update(&OpenOrder {
                symbol: "BTCUSDT".into(),
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: grid_core::types::Side::Buy,
                price: 94.5,
                qty: 0.25,
                realized_pnl: -5.0,
                status: "PARTIALLY_FILLED".into(),
            })
            .unwrap();

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(
            instance.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((instance.risk_state.realized_pnl_today + 5.0).abs() < f64::EPSILON);
    }
}
