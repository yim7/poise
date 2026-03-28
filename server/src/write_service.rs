use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::events::DomainEvent;
use grid_engine::command::GridCommand;
use grid_engine::grid::{GridId, Instrument};
use grid_engine::manager::{GridManager, SubmitRecoveryPlan, SubmitRecoveryResolution};
use grid_engine::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use grid_engine::ports::{
    EffectStatusUpdate, ExchangeOrder, OrderReceipt, OrderRequest, StateRepositoryPort,
};
use grid_engine::runtime::SubmitRecoveryAnchor;
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

impl TransitionResult for SubmitRecoveryPlan {
    fn domain_events(&self) -> &[DomainEvent] {
        &[]
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

    pub async fn sync_exchange_state(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        submit_recovery_anchor: Option<SubmitRecoveryAnchor>,
    ) -> Result<GridTransition> {
        self.mutate_grid(id, |manager| {
            manager.sync_exchange_state(
                &GridId::new(id),
                position.clone(),
                open_orders.clone(),
                submit_recovery_anchor.clone(),
            )
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn record_submit_receipt(
        &self,
        id: &str,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        self.mutate_grid(id, |manager| {
            manager.record_submit_receipt(
                &GridId::new(id),
                request,
                target_exposure.clone(),
                receipt,
            )?;
            Ok(())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn clear_pending_submit(&self, id: &str, client_order_id: &str) -> Result<()> {
        self.mutate_grid(id, |manager| {
            manager.clear_pending_submit(&GridId::new(id), client_order_id)?;
            Ok(())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryResolution> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let (previous_snapshot, plan, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            let plan = manager
                .recover_submit_effect(
                    &GridId::new(id),
                    request,
                    target_exposure.clone(),
                    live_order,
                )
                .map_err(GridMutationError::Mutation)?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            (previous_snapshot, plan, next_snapshot)
        };

        let effect_status_update = match plan.resolution {
            SubmitRecoveryResolution::Succeeded => {
                Some(EffectStatusUpdate::succeeded(effect_id.to_string()))
            }
            SubmitRecoveryResolution::Superseded => {
                Some(EffectStatusUpdate::superseded(effect_id.to_string()))
            }
            SubmitRecoveryResolution::Proceed | SubmitRecoveryResolution::AwaitExchangeState => {
                None
            }
        };

        self.commit_grid_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &plan,
            effect_status_update.as_ref(),
            true,
        )
        .await
        .map_err(anyhow::Error::new)?;

        Ok(plan.resolution)
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

        self.commit_grid_mutation(id, &previous_snapshot, &next_snapshot, &result, None, false)
            .await?;

        Ok(result)
    }

    async fn commit_grid_mutation<R>(
        &self,
        id: &str,
        previous_snapshot: &grid_engine::snapshot::GridRuntimeSnapshot,
        next_snapshot: &grid_engine::snapshot::GridRuntimeSnapshot,
        result: &R,
        effect_status_update: Option<&EffectStatusUpdate>,
        skip_when_noop: bool,
    ) -> std::result::Result<(), GridMutationError>
    where
        R: TransitionResult,
    {
        let has_persistence_work = previous_snapshot != next_snapshot
            || !result.domain_events().is_empty()
            || !result.effects().is_empty()
            || effect_status_update.is_some();
        if skip_when_noop && !has_persistence_work {
            return Ok(());
        }

        if let Err(error) = self
            .repository
            .save_transition_with_effect_status(
                id,
                next_snapshot,
                result.domain_events(),
                result.effects(),
                effect_status_update,
            )
            .await
        {
            let rollback_result = {
                let mut manager = self.manager.write().await;
                manager.restore_grid_state(previous_snapshot)
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
        if effect_status_update.is_some() {
            self.emit_internal_notification(GridInternalNotification::GridEffectStateChanged {
                grid_id: GridId::new(id),
            });
        }

        Ok(())
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
    use grid_core::types::{ExchangeRules, Exposure, Side};
    use grid_engine::command::GridCommand;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::{GridManager, SubmitRecoveryResolution};
    use grid_engine::observation::{
        GridObservation, MarketObservation, OrderObservation, PositionObservation,
    };
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, EffectStatusUpdate, OrderRequest, OrderStatus,
        PersistedGridEffect, StateRepositoryPort,
    };
    use grid_engine::runtime::PendingOrder;
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

        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
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
    async fn sync_exchange_state_persists_single_startup_snapshot_write() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        service
            .sync_exchange_state(
                "btc-core",
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
                    status: grid_engine::ports::OrderStatus::New,
                }],
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .and_then(|snapshot| snapshot.pending_order)
                .and_then(|pending| pending.order_id),
            Some("live-1".into())
        );
    }

    #[tokio::test]
    async fn recover_submit_effect_proceed_persists_submitting_anchor_before_returning() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let transition = service.observe_market("btc-core", 95.0).await.unwrap();
        let (request, target_exposure) = match transition.effects.as_slice() {
            [
                GridEffect::SubmitOrder {
                    request,
                    target_exposure,
                },
            ] => (request.clone(), target_exposure.clone()),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        let effect_id = repository.pending_effects()[0].effect_id.clone();

        let resolution = service
            .recover_submit_effect(
                "btc-core",
                &effect_id,
                &request,
                target_exposure.clone(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(resolution, SubmitRecoveryResolution::Proceed);
        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .and_then(|snapshot| snapshot.pending_order),
            Some(PendingOrder::from_submit_request(
                &request,
                target_exposure.clone()
            ))
        );
        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        assert_eq!(
            manager
                .snapshot("btc-core")
                .and_then(|snapshot| snapshot.pending_order),
            Some(PendingOrder::from_submit_request(&request, target_exposure))
        );
    }

    #[tokio::test]
    async fn recover_submit_effect_supersedes_old_effect_in_same_persisted_write() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            client_order_id: "btc-core-reconcile".into(),
            side: Side::Buy,
            price: 94.0,
            quantity: snapshot.config.base_qty_per_unit() * 6.0,
        };
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.pending_order = Some(PendingOrder {
            order_id: None,
            client_order_id: request.client_order_id.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            target_exposure: Exposure(6.0),
            status: OrderStatus::Submitting,
        });
        snapshot.observed.reference_price = Some(95.0);
        {
            let mut manager = manager_handle.write().await;
            manager.restore_grid_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot.clone());

        let transition = service
            .observe_position(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
            )
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![GridEffect::NoOp]);
        repository.seed_effect(PersistedGridEffect {
            effect_id: "btc-core:recovery:0".into(),
            grid_id: GridId::new("btc-core"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: GridEffect::SubmitOrder {
                request: request.clone(),
                target_exposure: Exposure(6.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        let resolution = service
            .recover_submit_effect(
                "btc-core",
                "btc-core:recovery:0",
                &request,
                Exposure(6.0),
                None,
            )
            .await
            .unwrap();

        assert_eq!(resolution, SubmitRecoveryResolution::Superseded);
        let effects = repository.all_effects();
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects
                .iter()
                .find(|effect| effect.effect_id == "btc-core:recovery:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
        let replacement = effects
            .iter()
            .find(|effect| effect.effect_id != "btc-core:recovery:0")
            .expect("replacement submit effect should be persisted");
        assert_eq!(replacement.status, EffectStatus::Pending);
        assert!(matches!(
            &replacement.effect,
            GridEffect::SubmitOrder {
                request,
                target_exposure,
            } if request.side == Side::Buy
                && (request.price - 95.0).abs() < f64::EPSILON
                && (request.quantity - snapshot.config.base_qty_per_unit() * 4.0).abs() < f64::EPSILON
                && *target_exposure == Exposure(4.0)
        ));
        let replacement_pending = match &replacement.effect {
            GridEffect::SubmitOrder {
                request,
                target_exposure,
            } => Some(PendingOrder::from_submit_request(
                request,
                target_exposure.clone(),
            )),
            _ => None,
        };
        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .and_then(|snapshot| snapshot.pending_order),
            replacement_pending
        );

        let follow_up = service.observe_market("btc-core", 94.9).await.unwrap();
        assert_eq!(
            follow_up.effects,
            vec![GridEffect::NoOp],
            "replacement submit should keep suppressing duplicate submit plans before worker pickup"
        );
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

        fn all_effects(&self) -> Vec<PersistedGridEffect> {
            self.effects.lock().unwrap().clone()
        }

        fn seed_snapshot(&self, id: &str, snapshot: GridRuntimeSnapshot) {
            self.snapshots
                .lock()
                .unwrap()
                .insert(id.to_string(), snapshot);
        }

        fn seed_effect(&self, effect: PersistedGridEffect) {
            self.effects.lock().unwrap().push(effect);
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &GridRuntimeSnapshot,
            events: &[DomainEvent],
            effects: &[GridEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
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

            if let Some(effect_status_update) = effect_status_update {
                let effect = effect_store
                    .iter_mut()
                    .find(|effect| effect.effect_id == effect_status_update.effect_id)
                    .ok_or_else(|| {
                        anyhow!("effect `{}` not found", effect_status_update.effect_id)
                    })?;
                effect.status = effect_status_update.status;
                effect.attempt_count += effect_status_update.attempt_delta;
                effect.last_error = effect_status_update.last_error.clone();
                effect.updated_at = now;
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

        async fn mark_effect_superseded(&self, effect_id: &str) -> Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .ok_or_else(|| anyhow!("effect `{effect_id}` not found"))?;
            effect.status = EffectStatus::Superseded;
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
        async fn save_transition_with_effect_status(
            &self,
            _id: &str,
            _state: &GridRuntimeSnapshot,
            _events: &[DomainEvent],
            _effects: &[GridEffect],
            _effect_status_update: Option<&EffectStatusUpdate>,
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

        async fn mark_effect_superseded(&self, _effect_id: &str) -> Result<()> {
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
