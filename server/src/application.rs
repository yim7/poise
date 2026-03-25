use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::events::DomainEvent;
use grid_engine::command::GridCommand;
use grid_engine::grid::{GridId, Instrument};
use grid_engine::manager::GridManager;
use grid_engine::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use grid_engine::ports::StateRepositoryPort;
use grid_engine::runtime::{GridRuntime, GridStatus, PendingOrder};
use grid_engine::transition::{GridEffect, GridTransition};
use grid_protocol::{
    BandBoundary as ProtocolBandBoundary, DomainEvent as ProtocolDomainEvent,
    GridConfig as ProtocolGridConfig, GridSnapshot as ProtocolGridSnapshot,
    GridStatus as ProtocolGridStatus, GridSummary, OrderStatus as ProtocolOrderStatus,
    OutOfBandPolicy as ProtocolOutOfBandPolicy, PendingOrder as ProtocolPendingOrder,
    ShapeFamily as ProtocolShapeFamily, Side as ProtocolSide, WsEvent,
};
use tokio::sync::{Mutex, RwLock, broadcast};

pub type SharedManager = Arc<RwLock<GridManager>>;

#[derive(Clone)]
pub struct GridPlatformService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_lock: Arc<Mutex<()>>,
    events: broadcast::Sender<WsEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridInstrument {
    pub id: String,
    pub instrument: Instrument,
}

#[derive(Debug)]
pub enum GridMutationError {
    Mutation(anyhow::Error),
    Persistence(anyhow::Error),
}

impl std::fmt::Display for GridMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for GridMutationError {}

impl GridMutationError {
    pub fn message(&self) -> String {
        match self {
            Self::Mutation(error) | Self::Persistence(error) => error.to_string(),
        }
    }
}

pub(crate) trait TransitionResult {
    fn domain_events(&self) -> &[DomainEvent];
    fn effects(&self) -> &[GridEffect];
}

impl TransitionResult for () {
    fn domain_events(&self) -> &[DomainEvent] {
        &[]
    }

    fn effects(&self) -> &[GridEffect] {
        &[]
    }
}

impl TransitionResult for GridTransition {
    fn domain_events(&self) -> &[DomainEvent] {
        &self.events
    }

    fn effects(&self) -> &[GridEffect] {
        &self.effects
    }
}

impl GridPlatformService {
    pub fn new(
        manager: GridManager,
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

    pub(crate) fn repository(&self) -> Arc<dyn StateRepositoryPort> {
        Arc::clone(&self.repository)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<WsEvent> {
        self.events.subscribe()
    }

    pub async fn has_grid(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_grid(id).is_some()
    }

    pub async fn list_grid_summaries(&self) -> Vec<GridSummary> {
        let manager = self.manager.read().await;
        manager
            .list_grids()
            .into_iter()
            .map(protocol_grid_summary)
            .collect()
    }

    pub async fn grid_snapshot(&self, id: &str) -> Option<ProtocolGridSnapshot> {
        let manager = self.manager.read().await;
        manager.get_grid(id).map(protocol_grid_snapshot)
    }

    pub async fn grid_instruments(&self) -> Vec<GridInstrument> {
        let manager = self.manager.read().await;
        manager
            .list_grids()
            .into_iter()
            .map(|grid| GridInstrument {
                id: grid.id.as_str().to_string(),
                instrument: grid.instrument.clone(),
            })
            .collect()
    }

    pub async fn resolve_grid_id(&self, instrument: &Instrument) -> Option<String> {
        let manager = self.manager.read().await;
        manager
            .resolve_grid_id(instrument)
            .map(|grid_id| grid_id.as_str().to_string())
    }

    pub async fn observe_market(&self, id: &str, reference_price: f64) -> Result<GridTransition> {
        self.mutate_grid(id, |manager| {
            manager.observe(
                &GridId::new(id),
                GridObservation::Market(MarketObservation { reference_price }),
            )
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn command(&self, id: &str, command: GridCommand) -> Result<GridTransition> {
        self.mutate_grid(id, |manager| {
            manager.command(&GridId::new(id), command.clone())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<GridTransition> {
        self.mutate_grid(id, |manager| {
            manager.observe(
                &GridId::new(id),
                GridObservation::Position(observation.clone()),
            )
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn observe_order(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<GridTransition> {
        self.mutate_grid(id, |manager| {
            manager.observe(
                &GridId::new(id),
                GridObservation::Order(observation.clone()),
            )
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn record_pending_order(&self, id: &str, pending: PendingOrder) -> Result<()> {
        self.mutate_grid(id, |manager| {
            manager.record_submitted_order(&GridId::new(id), pending.clone())?;
            Ok(())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn clear_pending_order(&self, id: &str) -> Result<()> {
        self.mutate_grid(id, |manager| manager.clear_pending_order(&GridId::new(id)))
            .await
            .map_err(anyhow::Error::new)
    }

    async fn mutate_grid<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, GridMutationError>
    where
        F: FnOnce(&mut GridManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.mutation_lock.lock().await;
        let (previous_snapshot, result, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            let result = mutate(&mut manager).map_err(GridMutationError::Mutation)?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            (previous_snapshot, result, next_snapshot)
        };

        if let Err(error) = self
            .repository
            .save_transition(id, &next_snapshot, result.domain_events(), result.effects())
            .await
        {
            let rollback_result = {
                let mut manager = self.manager.write().await;
                manager.restore_grid_state(&previous_snapshot)
            };
            if let Err(rollback_error) = rollback_result {
                return Err(GridMutationError::Persistence(anyhow!(
                    "failed to persist grid `{id}`: {error}; rollback failed: {rollback_error}"
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

        if result.domain_events().is_empty() && previous_snapshot != next_snapshot {
            let _ = self.events.send(WsEvent {
                grid_id: id.to_string(),
                event: ProtocolDomainEvent::SnapshotUpdated,
            });
        }

        Ok(result)
    }
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
        DomainEvent::BandReentered { price } => {
            ProtocolDomainEvent::BandReentered { price: *price }
        }
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

fn protocol_grid_summary(value: &GridRuntime) -> GridSummary {
    GridSummary {
        id: value.id.as_str().to_string(),
        symbol: value.symbol().to_string(),
        status: protocol_grid_status(value.status.clone()),
        reference_price: value.reference_price,
    }
}

fn protocol_grid_snapshot(value: &GridRuntime) -> ProtocolGridSnapshot {
    ProtocolGridSnapshot {
        id: value.id.as_str().to_string(),
        symbol: value.symbol().to_string(),
        status: protocol_grid_status(value.status.clone()),
        current_exposure: value.current_exposure.0,
        target_exposure: value.target_exposure.as_ref().map(|exposure| exposure.0),
        reference_price: value.reference_price,
        pending_order: value
            .pending_order
            .as_ref()
            .map(|pending_order| protocol_pending_order(value.symbol(), pending_order)),
        config: protocol_grid_config(&value.config),
    }
}

fn protocol_grid_status(value: GridStatus) -> ProtocolGridStatus {
    match value {
        GridStatus::WaitingMarketData => ProtocolGridStatus::WaitingMarketData,
        GridStatus::Active => ProtocolGridStatus::Active,
        GridStatus::Frozen => ProtocolGridStatus::Frozen,
        GridStatus::ReducingOnly => ProtocolGridStatus::ReducingOnly,
        GridStatus::Holding => ProtocolGridStatus::Holding,
        GridStatus::Terminated => ProtocolGridStatus::Terminated,
        GridStatus::Paused => ProtocolGridStatus::Paused,
    }
}

fn protocol_pending_order(symbol: &str, value: &PendingOrder) -> ProtocolPendingOrder {
    ProtocolPendingOrder {
        symbol: symbol.to_string(),
        order_id: value.order_id.clone(),
        client_order_id: value.client_order_id.clone(),
        side: protocol_side(value.side),
        price: value.price,
        quantity: value.quantity,
        status: protocol_order_status(value.status),
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

fn protocol_order_status(value: grid_engine::ports::OrderStatus) -> ProtocolOrderStatus {
    match value {
        grid_engine::ports::OrderStatus::Submitting => ProtocolOrderStatus::Submitting,
        grid_engine::ports::OrderStatus::New => ProtocolOrderStatus::New,
        grid_engine::ports::OrderStatus::PartiallyFilled => ProtocolOrderStatus::PartiallyFilled,
        grid_engine::ports::OrderStatus::Filled => ProtocolOrderStatus::Filled,
        grid_engine::ports::OrderStatus::Canceling => ProtocolOrderStatus::Canceling,
        grid_engine::ports::OrderStatus::Canceled => ProtocolOrderStatus::Canceled,
        grid_engine::ports::OrderStatus::Rejected => ProtocolOrderStatus::Rejected,
        grid_engine::ports::OrderStatus::Expired => ProtocolOrderStatus::Expired,
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
    use grid_core::types::ExchangeRules;
    use grid_engine::command::GridCommand;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::observation::{GridObservation, MarketObservation};
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, PersistedGridEffect, StateRepositoryPort,
    };
    use grid_engine::snapshot::GridRuntimeSnapshot;
    use grid_engine::transition::GridEffect;
    use grid_protocol::{
        DomainEvent as ProtocolDomainEvent, GridSnapshot as ProtocolGridSnapshot,
        GridStatus as ProtocolGridStatus, GridSummary,
    };

    use super::{GridPlatformService, protocol_domain_event};

    #[tokio::test]
    async fn mutate_grid_persists_tick_events_and_broadcasts_after_save() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_events();

        let outcome = service
            .mutate_grid("btc-core", |manager| {
                manager.observe(
                    &GridId::new("btc-core"),
                    GridObservation::Market(MarketObservation {
                        reference_price: 95.0,
                    }),
                )
            })
            .await
            .unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(repository.events_for("btc-core"), outcome.events);

        let broadcast = receiver.recv().await.unwrap();
        assert_eq!(broadcast.grid_id, "btc-core");
        assert_eq!(broadcast.event, protocol_domain_event(&outcome.events[0]));
    }

    #[tokio::test]
    async fn mutate_grid_persists_engine_snapshot_without_server_side_snapshot_builder() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        service
            .mutate_grid("btc-core", |manager| {
                manager.observe(
                    &GridId::new("btc-core"),
                    GridObservation::Market(MarketObservation {
                        reference_price: 95.0,
                    }),
                )
            })
            .await
            .unwrap();

        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        let expected = manager.get_grid("btc-core").unwrap().snapshot();

        assert_eq!(repository.snapshot_for("btc-core"), Some(expected));
    }

    #[tokio::test]
    async fn mutate_grid_persists_effects_with_snapshot_and_events() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let transition = service.observe_market("btc-core", 95.0).await.unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, GridEffect::SubmitOrder { .. }))
        );

        let pending = repository.pending_effects();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].grid_id.as_str(), "btc-core");
        assert!(matches!(pending[0].effect, GridEffect::SubmitOrder { .. }));
        assert_eq!(pending[0].status, EffectStatus::Pending);
        assert_eq!(repository.events_for("btc-core"), transition.events);
        assert!(repository.snapshot_for("btc-core").is_some());
    }

    #[tokio::test]
    async fn mutate_grid_rolls_back_and_does_not_broadcast_when_save_fails() {
        let repository = Arc::new(FailOnSaveRepository);
        let service = test_service(repository as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_events();

        let error = match service
            .mutate_grid("btc-core", |manager| {
                manager.observe(
                    &GridId::new("btc-core"),
                    GridObservation::Market(MarketObservation {
                        reference_price: 95.0,
                    }),
                )
            })
            .await
        {
            Ok(_) => panic!("mutation should fail when save fails"),
            Err(error) => error,
        };
        assert!(matches!(error, super::GridMutationError::Persistence(_)));

        let snapshot = service.grid_snapshot("btc-core").await.unwrap();
        assert_eq!(snapshot.target_exposure, None);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), receiver.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn mutate_grid_broadcasts_snapshot_updated_when_state_changes_without_domain_events() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_events();

        service
            .mutate_grid("btc-core", |manager| {
                manager.command(&GridId::new("btc-core"), GridCommand::Pause)
            })
            .await
            .unwrap();

        let broadcast = receiver.recv().await.unwrap();
        assert_eq!(broadcast.grid_id, "btc-core");
        assert_eq!(broadcast.event, ProtocolDomainEvent::SnapshotUpdated);
    }

    #[tokio::test]
    async fn exposes_protocol_read_models_without_http_side_mapping() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);

        let summaries: Vec<GridSummary> = service.list_grid_summaries().await;
        let snapshot: ProtocolGridSnapshot = service.grid_snapshot("btc-core").await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "btc-core");
        assert_eq!(summaries[0].status, ProtocolGridStatus::WaitingMarketData);
        assert_eq!(snapshot.id, "btc-core");
        assert_eq!(snapshot.status, ProtocolGridStatus::WaitingMarketData);
    }

    #[tokio::test]
    async fn resolves_grid_id_from_instrument() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);

        let grid_id = service
            .resolve_grid_id(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await;

        assert_eq!(grid_id, Some("btc-core".to_string()));
    }

    fn test_service(repository: Arc<dyn StateRepositoryPort>) -> GridPlatformService {
        let (events, _) = tokio::sync::broadcast::channel(16);
        let mut manager = GridManager::new(Arc::new(FixedClock));
        manager
            .add_grid(
                GridId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
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
        snapshots: Mutex<HashMap<String, GridRuntimeSnapshot>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedGridEffect>>,
        next_effect_seq: Mutex<u64>,
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

        fn snapshot_for(&self, id: &str) -> Option<GridRuntimeSnapshot> {
            self.snapshots.lock().unwrap().get(id).cloned()
        }

        fn pending_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition(
            &self,
            id: &str,
            state: &GridRuntimeSnapshot,
            events: &[DomainEvent],
            effects: &[GridEffect],
        ) -> Result<CommittedGridWrite> {
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

            let now = Utc::now();
            let mut next_effect_seq = self.next_effect_seq.lock().unwrap();
            let mut effect_store = self.effects.lock().unwrap();
            let mut persisted_effects = Vec::new();
            *next_effect_seq += 1;
            let batch_id = next_effect_seq.to_string();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, GridEffect::NoOp) {
                    continue;
                }

                let persisted = PersistedGridEffect {
                    effect_id: format!("{id}:{batch_id}:{sequence}"),
                    grid_id: GridId::new(id),
                    batch_id: batch_id.clone(),
                    sequence: u32::try_from(sequence).unwrap(),
                    effect: effect.clone(),
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                };
                effect_store.push(persisted.clone());
                persisted_effects.push(persisted);
            }

            Ok(CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(&self, id: &str) -> Result<Option<GridRuntimeSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
        }

        async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
            Ok(self.events_for(id))
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(self.pending_effects())
        }

        async fn mark_effect_executing(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Executing;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_succeeded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Succeeded;
            effect.last_error = None;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_failed(&self, effect_id: &str, error: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Failed;
            effect.attempt_count += 1;
            effect.last_error = Some(error.to_string());
            effect.updated_at = Utc::now();
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailOnSaveRepository;

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSaveRepository {
        async fn save_transition(
            &self,
            _id: &str,
            _state: &GridRuntimeSnapshot,
            _events: &[DomainEvent],
            _effects: &[GridEffect],
        ) -> Result<CommittedGridWrite> {
            Err(anyhow!("injected save failure"))
        }

        async fn load_grid_state(&self, _id: &str) -> Result<Option<GridRuntimeSnapshot>> {
            Ok(None)
        }

        async fn list_events(&self, _id: &str) -> Result<Vec<DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(Vec::new())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> Result<()> {
            Ok(())
        }

        async fn mark_effect_succeeded(&self, _effect_id: &str) -> Result<()> {
            Ok(())
        }

        async fn mark_effect_failed(&self, _effect_id: &str, _error: &str) -> Result<()> {
            Ok(())
        }
    }

    struct FixedClock;

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()
        }
    }

    #[allow(dead_code)]
    fn _test_snapshot() -> GridRuntimeSnapshot {
        test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>)
            .manager
            .blocking_read()
            .get_grid("btc-core")
            .unwrap()
            .snapshot()
    }
}
