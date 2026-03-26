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
use grid_engine::runtime::PendingOrder;
use grid_engine::snapshot::GridRuntimeSnapshot;
use grid_engine::transition::{GridEffect, GridTransition};
use tokio::sync::{Mutex, RwLock, broadcast};

use crate::notifications::GridInternalNotification;

pub type SharedManager = Arc<RwLock<GridManager>>;

#[derive(Clone)]
pub struct GridWriteService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_lock: Arc<Mutex<()>>,
    notifications: broadcast::Sender<GridInternalNotification>,
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

impl GridWriteService {
    pub fn new(
        manager: GridManager,
        repository: Arc<dyn StateRepositoryPort>,
        notifications: broadcast::Sender<GridInternalNotification>,
    ) -> Self {
        Self {
            manager: Arc::new(RwLock::new(manager)),
            repository,
            mutation_lock: Arc::new(Mutex::new(())),
            notifications,
        }
    }

    #[cfg(test)]
    pub fn manager(&self) -> SharedManager {
        Arc::clone(&self.manager)
    }

    pub(crate) fn repository(&self) -> Arc<dyn StateRepositoryPort> {
        Arc::clone(&self.repository)
    }

    pub fn subscribe_notifications(&self) -> broadcast::Receiver<GridInternalNotification> {
        self.notifications.subscribe()
    }

    pub(crate) fn emit_internal_notification(&self, notification: GridInternalNotification) {
        let _ = self.notifications.send(notification);
    }

    pub async fn has_grid(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_grid(id).is_some()
    }

    pub async fn list_grid_snapshots(&self) -> Vec<GridRuntimeSnapshot> {
        let manager = self.manager.read().await;
        manager
            .list_grids()
            .into_iter()
            .map(|grid| grid.snapshot())
            .collect()
    }

    pub async fn grid_snapshot(&self, id: &str) -> Option<GridRuntimeSnapshot> {
        let manager = self.manager.read().await;
        manager.snapshot(id)
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

        self.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
            grid_id: GridId::new(id),
        });

        Ok(result)
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

    use crate::notifications::GridInternalNotification;

    use super::GridWriteService;

    #[tokio::test]
    async fn mutate_grid_persists_tick_events_and_emits_notification_after_save() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

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

        let notification = receiver.recv().await.unwrap();
        assert_eq!(
            notification,
            GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new("btc-core"),
            }
        );
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
        let mut receiver = service.subscribe_notifications();

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
    async fn command_persists_transition_and_emits_grid_write_committed() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

        service
            .command("btc-core", GridCommand::Pause)
            .await
            .unwrap();

        let notification = receiver.recv().await.unwrap();
        assert_eq!(
            notification,
            GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new("btc-core"),
            }
        );
    }

    #[tokio::test]
    async fn exposes_internal_snapshots_without_protocol_mapping() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);

        let snapshots = service.list_grid_snapshots().await;
        let snapshot = service.grid_snapshot("btc-core").await.unwrap();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].grid_id.as_str(), "btc-core");
        assert_eq!(snapshots[0].status, snapshot.status);
        assert_eq!(snapshot.grid_id.as_str(), "btc-core");
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

    fn test_service(repository: Arc<dyn StateRepositoryPort>) -> GridWriteService {
        let (notifications, _) = tokio::sync::broadcast::channel(16);
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

        GridWriteService::new(manager, repository, notifications)
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
