use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::events::DomainEvent;
use grid_protocol::{
    BandBoundary as ProtocolBandBoundary, DomainEvent as ProtocolDomainEvent,
    GridConfig as ProtocolGridConfig, GridSnapshot as ProtocolGridSnapshot,
    GridStatus as ProtocolGridStatus, GridSummary, OutOfBandPolicy as ProtocolOutOfBandPolicy,
    PendingOrder as ProtocolPendingOrder, ShapeFamily as ProtocolShapeFamily,
    Side as ProtocolSide, WsEvent,
};
use grid_engine::instance::StrategyInstance;
use grid_engine::manager::{InstanceManager, TickOutcome};
use grid_engine::instance::PendingOrder;
use grid_engine::ports::{GridSnapshot, StateRepositoryPort};
use tokio::sync::{Mutex, RwLock, broadcast};

pub type SharedManager = Arc<RwLock<InstanceManager>>;

#[derive(Clone)]
pub struct GridPlatformService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_lock: Arc<Mutex<()>>,
    events: broadcast::Sender<WsEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridBinding {
    pub id: String,
    pub symbol: String,
}

#[derive(Debug)]
pub enum GridMutationError {
    Mutation(anyhow::Error),
    Persistence(anyhow::Error),
}

impl GridMutationError {
    pub fn message(&self) -> String {
        match self {
            Self::Mutation(error) | Self::Persistence(error) => error.to_string(),
        }
    }
}

pub(crate) trait TransitionResult {
    fn domain_events(&self) -> &[DomainEvent];
}

impl TransitionResult for () {
    fn domain_events(&self) -> &[DomainEvent] {
        &[]
    }
}

impl TransitionResult for TickOutcome {
    fn domain_events(&self) -> &[DomainEvent] {
        &self.events
    }
}

impl GridPlatformService {
    pub fn new(
        manager: InstanceManager,
        repository: Arc<dyn StateRepositoryPort>,
        events: broadcast::Sender<WsEvent>,
    ) -> Self {
        Self {
            manager: Arc::new(RwLock::new(manager)),
            repository,
            mutation_lock: Arc::new(Mutex::new(())),
            events,
        }
    }

    #[cfg(test)]
    pub fn manager(&self) -> SharedManager {
        Arc::clone(&self.manager)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<WsEvent> {
        self.events.subscribe()
    }

    pub async fn has_grid(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_instance(id).is_some()
    }

    pub async fn list_grid_summaries(&self) -> Vec<GridSummary> {
        let manager = self.manager.read().await;
        manager
            .list_instances()
            .into_iter()
            .map(protocol_grid_summary)
            .collect()
    }

    pub async fn grid_snapshot(&self, id: &str) -> Option<ProtocolGridSnapshot> {
        let manager = self.manager.read().await;
        manager.get_instance(id).map(protocol_grid_snapshot)
    }

    pub async fn grid_bindings(&self) -> Vec<GridBinding> {
        let manager = self.manager.read().await;
        manager
            .list_instances()
            .into_iter()
            .map(|grid| GridBinding {
                id: grid.id.clone(),
                symbol: grid.symbol.clone(),
            })
            .collect()
    }

    pub async fn grid_id_for_symbol(&self, symbol: &str) -> Option<String> {
        let manager = self.manager.read().await;
        manager
            .list_instances()
            .into_iter()
            .find(|grid| grid.symbol == symbol)
            .map(|grid| grid.id.clone())
    }

    pub async fn reconcile_context_for_symbol(&self, symbol: &str) -> Option<(String, f64)> {
        let manager = self.manager.read().await;
        manager.list_instances().into_iter().find_map(|grid| {
            if grid.symbol == symbol {
                grid.reference_price
                    .map(|reference_price| (grid.id.clone(), reference_price))
            } else {
                None
            }
        })
    }

    pub(crate) async fn mutate_grid<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, GridMutationError>
    where
        F: FnOnce(&mut InstanceManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.mutation_lock.lock().await;
        let (previous_snapshot, result, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = snapshot_for_id(&manager, id).map_err(GridMutationError::Mutation)?;
            let result = mutate(&mut manager).map_err(GridMutationError::Mutation)?;
            let next_snapshot = snapshot_for_id(&manager, id).map_err(GridMutationError::Mutation)?;
            (previous_snapshot, result, next_snapshot)
        };

        if let Err(error) = self
            .repository
            .save_transition(id, &next_snapshot, result.domain_events())
            .await
        {
            let rollback_result = {
                let mut manager = self.manager.write().await;
                manager.restore_instance_state(&previous_snapshot)
            };
            if let Err(rollback_error) = rollback_result {
                return Err(GridMutationError::Persistence(anyhow!(
                    "failed to persist instance `{id}`: {error}; rollback failed: {rollback_error}"
                )));
            }
            return Err(GridMutationError::Persistence(error));
        }

        for event in result.domain_events() {
            let _ = self.events.send(WsEvent {
                grid_id: id.to_string(),
                event: protocol_domain_event(event),
            });
        }

        Ok(result)
    }
}

pub(crate) fn snapshot_from_instance(instance: &StrategyInstance) -> GridSnapshot {
    GridSnapshot {
        id: instance.id.clone(),
        symbol: instance.symbol.clone(),
        config: instance.config.clone(),
        status: instance.status.clone(),
        current_exposure: instance.current_exposure.clone(),
        target_exposure: instance.target_exposure.clone(),
        pending_order: instance.pending_order.clone(),
        risk_state: instance.risk_state.clone(),
        reference_price: instance.reference_price,
        out_of_band_since: instance.out_of_band_since,
    }
}

fn snapshot_for_id(manager: &InstanceManager, id: &str) -> Result<GridSnapshot> {
    let instance = manager
        .get_instance(id)
        .ok_or_else(|| anyhow!("instance `{id}` not found"))?;
    Ok(snapshot_from_instance(instance))
}

fn protocol_domain_event(event: &DomainEvent) -> ProtocolDomainEvent {
    match event {
        DomainEvent::ExposureTargetChanged { from, to } => {
            ProtocolDomainEvent::ExposureTargetChanged {
                from: from.0,
                to: to.0,
            }
        }
        DomainEvent::BandBreached { boundary, price } => ProtocolDomainEvent::BandBreached {
            boundary: match boundary {
                grid_core::strategy::BandBoundary::Below => ProtocolBandBoundary::Below,
                grid_core::strategy::BandBoundary::Above => ProtocolBandBoundary::Above,
            },
            price: *price,
        },
        DomainEvent::BandReentered { price } => ProtocolDomainEvent::BandReentered {
            price: *price,
        },
        DomainEvent::PolicyTriggered { policy } => ProtocolDomainEvent::PolicyTriggered {
            policy: match policy {
                grid_core::strategy::OutOfBandPolicy::Freeze => ProtocolOutOfBandPolicy::Freeze,
                grid_core::strategy::OutOfBandPolicy::ReduceOnly => {
                    ProtocolOutOfBandPolicy::ReduceOnly
                }
                grid_core::strategy::OutOfBandPolicy::Terminate => {
                    ProtocolOutOfBandPolicy::Terminate
                }
                grid_core::strategy::OutOfBandPolicy::Hold => ProtocolOutOfBandPolicy::Hold,
            },
        },
        DomainEvent::RiskCapApplied { intended, capped } => ProtocolDomainEvent::RiskCapApplied {
            intended: intended.0,
            capped: capped.0,
        },
        DomainEvent::RiskDenied { reason } => ProtocolDomainEvent::RiskDenied {
            reason: reason.clone(),
        },
    }
}

fn protocol_grid_summary(value: &StrategyInstance) -> GridSummary {
    GridSummary {
        id: value.id.clone(),
        symbol: value.symbol.clone(),
        status: protocol_grid_status(value.status.clone()),
        reference_price: value.reference_price,
    }
}

fn protocol_grid_snapshot(value: &StrategyInstance) -> ProtocolGridSnapshot {
    ProtocolGridSnapshot {
        id: value.id.clone(),
        symbol: value.symbol.clone(),
        status: protocol_grid_status(value.status.clone()),
        current_exposure: value.current_exposure.0,
        target_exposure: value.target_exposure.as_ref().map(|exposure| exposure.0),
        reference_price: value.reference_price,
        pending_order: value.pending_order.as_ref().map(protocol_pending_order),
        config: protocol_grid_config(&value.config),
    }
}

fn protocol_grid_status(value: grid_engine::instance::GridStatus) -> ProtocolGridStatus {
    match value {
        grid_engine::instance::GridStatus::WaitingMarketData => ProtocolGridStatus::WaitingMarketData,
        grid_engine::instance::GridStatus::Active => ProtocolGridStatus::Active,
        grid_engine::instance::GridStatus::Frozen => ProtocolGridStatus::Frozen,
        grid_engine::instance::GridStatus::ReducingOnly => ProtocolGridStatus::ReducingOnly,
        grid_engine::instance::GridStatus::Holding => ProtocolGridStatus::Holding,
        grid_engine::instance::GridStatus::Terminated => ProtocolGridStatus::Terminated,
        grid_engine::instance::GridStatus::Paused => ProtocolGridStatus::Paused,
    }
}

fn protocol_pending_order(value: &PendingOrder) -> ProtocolPendingOrder {
    ProtocolPendingOrder {
        symbol: value.symbol.clone(),
        order_id: value.order_id.clone(),
        client_order_id: value.client_order_id.clone(),
        side: protocol_side(value.side),
        price: value.price,
        quantity: value.quantity,
        status: value.status.clone(),
    }
}

fn protocol_grid_config(value: &grid_core::strategy::GridConfig) -> ProtocolGridConfig {
    ProtocolGridConfig {
        lower_price: value.lower_price,
        upper_price: value.upper_price,
        long_exposure_units: value.long_exposure_units,
        short_exposure_units: value.short_exposure_units,
        notional_per_unit: value.notional_per_unit,
        shape_family: protocol_shape_family(value.shape_family),
        out_of_band_policy: protocol_out_of_band_policy(value.out_of_band_policy),
    }
}

fn protocol_side(value: grid_core::types::Side) -> ProtocolSide {
    match value {
        grid_core::types::Side::Buy => ProtocolSide::Buy,
        grid_core::types::Side::Sell => ProtocolSide::Sell,
    }
}

fn protocol_shape_family(value: grid_core::strategy::ShapeFamily) -> ProtocolShapeFamily {
    match value {
        grid_core::strategy::ShapeFamily::Linear => ProtocolShapeFamily::Linear,
        grid_core::strategy::ShapeFamily::Convex => ProtocolShapeFamily::Convex,
        grid_core::strategy::ShapeFamily::Concave => ProtocolShapeFamily::Concave,
    }
}

fn protocol_out_of_band_policy(
    value: grid_core::strategy::OutOfBandPolicy,
) -> ProtocolOutOfBandPolicy {
    match value {
        grid_core::strategy::OutOfBandPolicy::Freeze => ProtocolOutOfBandPolicy::Freeze,
        grid_core::strategy::OutOfBandPolicy::ReduceOnly => ProtocolOutOfBandPolicy::ReduceOnly,
        grid_core::strategy::OutOfBandPolicy::Terminate => ProtocolOutOfBandPolicy::Terminate,
        grid_core::strategy::OutOfBandPolicy::Hold => ProtocolOutOfBandPolicy::Hold,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use grid_core::events::DomainEvent;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure};
    use grid_engine::instance::RiskState;
    use grid_engine::manager::InstanceManager;
    use grid_engine::ports::{ClockPort, GridSnapshot, StateRepositoryPort};
    use grid_protocol::{GridSnapshot as ProtocolGridSnapshot, GridStatus as ProtocolGridStatus, GridSummary};

    use super::{GridPlatformService, protocol_domain_event};

    #[tokio::test]
    async fn mutate_grid_persists_tick_events_and_broadcasts_after_save() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_events();

        let tick = grid_engine::ports::PriceTick {
            symbol: "BTCUSDT".into(),
            reference_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let outcome = service
            .mutate_grid("BTCUSDT", |manager| manager.on_price_tick(&tick))
            .await
            .unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            repository.events_for("BTCUSDT"),
            outcome.events
        );

        let broadcast = receiver.recv().await.unwrap();
        assert_eq!(broadcast.grid_id, "BTCUSDT");
        assert_eq!(broadcast.event, protocol_domain_event(&outcome.events[0]));
    }

    #[tokio::test]
    async fn mutate_grid_rolls_back_and_does_not_broadcast_when_save_fails() {
        let repository = Arc::new(FailOnSaveRepository::default());
        let service = test_service(repository as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_events();

        let tick = grid_engine::ports::PriceTick {
            symbol: "BTCUSDT".into(),
            reference_price: 95.0,
            mark_price: 95.0,
            timestamp: Utc::now(),
        };

        let error = match service
            .mutate_grid("BTCUSDT", |manager| manager.on_price_tick(&tick))
            .await
        {
            Ok(_) => panic!("mutation should fail when save fails"),
            Err(error) => error,
        };
        assert!(matches!(error, super::GridMutationError::Persistence(_)));

        let snapshot = service.grid_snapshot("BTCUSDT").await.unwrap();
        assert_eq!(snapshot.target_exposure, None);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), receiver.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn exposes_protocol_read_models_without_http_side_mapping() {
        let service = test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);

        let summaries: Vec<GridSummary> = service.list_grid_summaries().await;
        let snapshot: ProtocolGridSnapshot = service.grid_snapshot("BTCUSDT").await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "BTCUSDT");
        assert_eq!(summaries[0].status, ProtocolGridStatus::WaitingMarketData);
        assert_eq!(snapshot.id, "BTCUSDT");
        assert_eq!(snapshot.status, ProtocolGridStatus::WaitingMarketData);
    }

    fn test_service(repository: Arc<dyn StateRepositoryPort>) -> GridPlatformService {
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut manager = InstanceManager::new(Arc::new(FixedClock));
        manager
            .add_grid(
                "BTCUSDT".into(),
                "BTCUSDT".into(),
                GridConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: OutOfBandPolicy::Freeze,
                },
                CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
                },
                ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                },
            )
            .unwrap();

        GridPlatformService::new(manager, repository, events)
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: Mutex<HashMap<String, GridSnapshot>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
    }

    impl MemoryRepository {
        fn events_for(&self, id: &str) -> Vec<DomainEvent> {
            self.events
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridSnapshot,
            events: &[DomainEvent],
        ) -> Result<()> {
            self.snapshots
                .lock()
                .unwrap()
                .insert(id.to_string(), state.clone());
            self.events
                .lock()
                .unwrap()
                .entry(id.to_string())
                .or_default()
                .extend_from_slice(events);
            Ok(())
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
        }

        async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
            Ok(self.events_for(id))
        }
    }

    #[derive(Default)]
    struct FailOnSaveRepository;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSaveRepository {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &GridSnapshot,
            _events: &[DomainEvent],
        ) -> Result<()> {
            Err(anyhow!("injected save failure"))
        }

        async fn load_grid_state(&self, _id: &str) -> Result<Option<GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<DomainEvent>> {
            Ok(Vec::new())
        }
    }

    struct FixedClock;

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()
        }
    }

    #[allow(dead_code)]
    fn _test_snapshot() -> GridSnapshot {
        GridSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: grid_engine::instance::GridStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: None,
            pending_order: None,
            risk_state: RiskState::default(),
            reference_price: Some(95.0),
            out_of_band_since: None,
        }
    }
}
