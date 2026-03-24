use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use grid_core::events::DomainEvent;
use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;

use crate::instance::StrategyInstance;
use crate::ports::{ClockPort, ExchangePort, PersistencePort, PriceTick};
use crate::reconciler;

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
    ) -> Result<()> {
        grid_core::strategy::validate_config(&config).map_err(|e| anyhow::anyhow!(e))?;
        let instance = StrategyInstance::new(id.clone(), symbol, config);
        self.instances.insert(id.clone(), instance);
        self.budgets.insert(id, budget);
        Ok(())
    }

    pub fn on_price_tick(&mut self, tick: &PriceTick) -> Vec<DomainEvent> {
        let mut all_events = vec![];
        let ids: Vec<String> = self
            .instances
            .keys()
            .filter(|id| self.instances[*id].symbol == tick.symbol)
            .cloned()
            .collect();

        for id in ids {
            let instance = self.instances.get(&id).unwrap();
            let budget = self.budgets.get(&id).unwrap();
            let result = reconciler::reconcile(instance, tick.last_price, budget);

            let instance = self.instances.get_mut(&id).unwrap();
            if let Some(new_status) = result.new_status {
                instance.status = new_status;
            }
            instance.current_exposure = result.target_exposure;
            instance.last_price = Some(tick.last_price);

            all_events.extend(result.plan.events);
        }
        all_events
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::InstanceStatus;
    use crate::ports::*;
    use chrono::Utc;
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
        async fn cancel_all(&self, _symbol: &str) -> Result<Vec<String>> {
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

    fn test_manager() -> InstanceManager {
        InstanceManager::new(
            Arc::new(FakeExchange),
            Arc::new(FakePersistence),
            Arc::new(FakeClock),
        )
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
                .add_instance("test".into(), "BTCUSDT".into(), bad_config, test_budget())
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
            )
            .unwrap();
        manager
            .add_instance(
                "eth1".into(),
                "ETHUSDT".into(),
                test_config(),
                test_budget(),
            )
            .unwrap();

        assert_eq!(manager.list_instances().len(), 2);
        assert!(manager.get_instance("btc1").is_some());
        assert!(manager.get_instance("eth1").is_some());
        assert!(manager.get_instance("nonexistent").is_none());
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
            )
            .unwrap();

        let tick = PriceTick {
            symbol: "BTCUSDT".into(),
            last_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let events = manager.on_price_tick(&tick);
        assert!(!events.is_empty());

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::Active);
        assert_eq!(instance.last_price, Some(95.0));
        assert!(instance.current_exposure.0 > 0.0); // should be long below center
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
            )
            .unwrap();

        let tick = PriceTick {
            symbol: "ETHUSDT".into(),
            last_price: 2500.0,
            mark_price: 2500.0,
            timestamp: Utc::now(),
        };

        let events = manager.on_price_tick(&tick);
        assert!(events.is_empty());

        let instance = manager.get_instance("btc1").unwrap();
        assert_eq!(instance.status, InstanceStatus::WaitingMarketData);
    }
}
