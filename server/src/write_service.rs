use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use grid_core::events::DomainEvent;
use grid_engine::command::GridCommand;
use grid_engine::executor::{SubmitRecoveryPlan, SubmitRecoveryResolution};
use grid_engine::grid::{GridId, Instrument};
use grid_engine::manager::GridManager;
use grid_engine::observation::{
    GridObservation, MarketObservation, OrderObservation, PositionObservation,
};
use grid_engine::ports::{
    EffectStatusUpdate, ExchangeOrder, OrderReceipt, OrderRequest, StateRepositoryPort,
};
use grid_engine::transition::{GridEffect, GridTransition};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast};

use crate::notifications::GridInternalNotification;

pub type SharedManager = Arc<RwLock<GridManager>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupSyncMode {
    RecoverOnly,
    RecoverAndReconcile,
}

impl StartupSyncMode {
    fn allows_follow_up_reconcile(self) -> bool {
        matches!(self, Self::RecoverAndReconcile)
    }
}

#[derive(Default)]
struct GridMutationGuards {
    locks: Mutex<HashMap<GridId, Arc<Mutex<()>>>>,
}

impl GridMutationGuards {
    async fn lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(
                locks
                    .entry(GridId::new(id))
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };

        lock.lock_owned().await
    }
}

#[derive(Clone)]
pub struct GridWriteService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_guards: Arc<GridMutationGuards>,
    notifications: broadcast::Sender<GridInternalNotification>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridInstrument {
    pub id: String,
    pub instrument: Instrument,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSubmitExecution {
    pub target_exposure: grid_core::types::Exposure,
}

#[derive(Debug)]
pub enum GridMutationError {
    LoadedGridInvariant { grid_id: String },
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
            Self::LoadedGridInvariant { grid_id } => format!(
                "loaded-grid invariant violated for effect writeback: grid `{grid_id}` is not loaded in write-side runtime"
            ),
            Self::Mutation(error) | Self::Persistence(error) => error.to_string(),
        }
    }

    fn loaded_grid_invariant(grid_id: &str) -> Self {
        Self::LoadedGridInvariant {
            grid_id: grid_id.to_string(),
        }
    }

    fn is_loaded_grid_invariant_violation(&self) -> bool {
        matches!(self, Self::LoadedGridInvariant { .. })
    }
}

pub(crate) fn is_loaded_grid_invariant_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<GridMutationError>()
        .is_some_and(GridMutationError::is_loaded_grid_invariant_violation)
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
            mutation_guards: Arc::new(GridMutationGuards::default()),
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

    pub async fn refresh_market_data_health(&self, id: &str) -> Result<GridTransition> {
        self.mutate_grid_skip_noop(id, |manager| {
            manager.refresh_market_data_health(&GridId::new(id))
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
    ) -> Result<GridTransition> {
        self.sync_exchange_state_inner(
            id,
            position,
            open_orders,
            StartupSyncMode::RecoverAndReconcile,
        )
        .await
    }

    pub async fn sync_exchange_state_without_reconcile(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
    ) -> Result<GridTransition> {
        self.sync_exchange_state_inner(id, position, open_orders, StartupSyncMode::RecoverOnly)
            .await
    }

    async fn sync_exchange_state_inner(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        mode: StartupSyncMode,
    ) -> Result<GridTransition> {
        let _mutation_guard = self.lock_grid_mutation(id).await;
        let pending_submit_hints = self
            .repository
            .list_pending_submit_effects_for_grid(&GridId::new(id))
            .await
            .map_err(GridMutationError::Persistence)?
            .into_iter()
            .filter_map(|effect| match effect.effect {
                GridEffect::SubmitOrder {
                    request,
                    target_exposure,
                } => Some(grid_engine::executor::PendingSubmitHint {
                    request,
                    target_exposure,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (previous_snapshot, transition, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            let transition = if mode.allows_follow_up_reconcile() {
                manager
                    .sync_exchange_state(
                        &GridId::new(id),
                        position,
                        open_orders,
                        pending_submit_hints,
                    )
                    .map_err(GridMutationError::Mutation)?
            } else {
                manager
                    .sync_exchange_state_without_reconcile(
                        &GridId::new(id),
                        position,
                        open_orders,
                        pending_submit_hints,
                    )
                    .map_err(GridMutationError::Mutation)?
            };
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::Mutation(anyhow!("grid `{id}` not found")))?;
            (previous_snapshot, transition, next_snapshot)
        };

        self.commit_grid_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &transition,
            None,
            false,
        )
        .await
        .map_err(anyhow::Error::new)?;

        Ok(transition)
    }

    pub async fn complete_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        self.mutate_grid_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |manager| {
                manager.record_submit_receipt(
                    &GridId::new(id),
                    request,
                    target_exposure.clone(),
                    receipt,
                )?;
                Ok(())
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn record_submit_failure(
        &self,
        id: &str,
        effect_id: &str,
        client_order_id: &str,
        error: &str,
    ) -> Result<()> {
        self.mutate_grid_with_effect_status(id, effect_status_failed(effect_id, error), |manager| {
            manager.record_submit_failure(&GridId::new(id), client_order_id)?;
            Ok(())
        })
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn record_cancel_order_success(
        &self,
        id: &str,
        effect_id: &str,
        order_id: &str,
    ) -> Result<()> {
        self.mutate_grid_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |manager| {
                manager.record_cancel_order_success(&GridId::new(id), order_id)?;
                Ok(())
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn record_cancel_all_success(&self, id: &str, effect_id: &str) -> Result<()> {
        self.mutate_grid_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |manager| {
                manager.record_cancel_all_success(&GridId::new(id))?;
                Ok(())
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.mutate_grid_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |_manager| Ok(()),
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn complete_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<()> {
        self.mutate_grid_with_effect_status(
            id,
            effect_status_failed(effect_id, error),
            |_manager| Ok(()),
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn prepare_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<Option<PreparedSubmitExecution>> {
        Ok(
            match self
                .recover_submit_effect(id, effect_id, request, target_exposure, live_order)
                .await?
            {
                SubmitRecoveryResolution::Proceed {
                    target_exposure, ..
                } => Some(PreparedSubmitExecution { target_exposure }),
                SubmitRecoveryResolution::Recovered { .. }
                | SubmitRecoveryResolution::Superseded { .. }
                | SubmitRecoveryResolution::AwaitExchangeState => None,
            },
        )
    }

    pub async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        target_exposure: grid_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryResolution> {
        let _mutation_guard = self.lock_grid_mutation(id).await;
        let (previous_snapshot, plan, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::loaded_grid_invariant(id))?;
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
                .ok_or_else(|| GridMutationError::loaded_grid_invariant(id))?;
            (previous_snapshot, plan, next_snapshot)
        };

        let effect_status_update = match plan.resolution {
            SubmitRecoveryResolution::Recovered { .. } => {
                Some(EffectStatusUpdate::succeeded(effect_id.to_string()))
            }
            SubmitRecoveryResolution::Superseded { .. } => {
                Some(EffectStatusUpdate::superseded(effect_id.to_string()))
            }
            SubmitRecoveryResolution::Proceed { .. }
            | SubmitRecoveryResolution::AwaitExchangeState => None,
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

    async fn mutate_grid_with_effect_status<R, F>(
        &self,
        id: &str,
        effect_status_update: EffectStatusUpdate,
        mutate: F,
    ) -> std::result::Result<R, GridMutationError>
    where
        F: FnOnce(&mut GridManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.lock_grid_mutation(id).await;
        let (previous_snapshot, result, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::loaded_grid_invariant(id))?;
            let result = mutate(&mut manager).map_err(GridMutationError::Mutation)?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| GridMutationError::loaded_grid_invariant(id))?;
            (previous_snapshot, result, next_snapshot)
        };

        self.commit_grid_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &result,
            Some(&effect_status_update),
            false,
        )
        .await?;
        Ok(result)
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
        self.mutate_grid_with_options(id, false, mutate).await
    }

    async fn mutate_grid_skip_noop<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, GridMutationError>
    where
        F: FnOnce(&mut GridManager) -> Result<R>,
        R: TransitionResult,
    {
        self.mutate_grid_with_options(id, true, mutate).await
    }

    async fn mutate_grid_with_options<R, F>(
        &self,
        id: &str,
        skip_when_noop: bool,
        mutate: F,
    ) -> std::result::Result<R, GridMutationError>
    where
        F: FnOnce(&mut GridManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.lock_grid_mutation(id).await;
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

        self.commit_grid_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &result,
            None,
            skip_when_noop,
        )
        .await?;
        Ok(result)
    }

    async fn lock_grid_mutation(&self, id: &str) -> OwnedMutexGuard<()> {
        self.mutation_guards.lock(id).await
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
        let has_grid_write = previous_snapshot != next_snapshot
            || !result.domain_events().is_empty()
            || !result.effects().is_empty();
        let has_effect_status_update = effect_status_update.is_some();
        let has_persistence_work = has_grid_write || has_effect_status_update;
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

        if has_grid_write {
            self.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new(id),
                recovery_anomaly_active: next_snapshot.executor_state.recovery_anomaly.is_some(),
            });
        }
        if has_effect_status_update {
            self.emit_internal_notification(GridInternalNotification::GridEffectStateChanged {
                grid_id: GridId::new(id),
            });
        }

        Ok(())
    }
}

fn effect_status_failed(effect_id: &str, error: &str) -> EffectStatusUpdate {
    EffectStatusUpdate {
        effect_id: effect_id.to_string(),
        status: grid_engine::ports::EffectStatus::Failed,
        attempt_delta: 1,
        last_error: Some(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use grid_core::events::DomainEvent;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::{ExchangeRules, Exposure, Side};
    use grid_engine::command::GridCommand;
    use grid_engine::executor::{ExecutionMode, OrderRole, OrderSlot, SubmitRecoveryResolution};
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::observation::{
        GridObservation, MarketObservation, OrderObservation, PositionObservation,
    };
    use grid_engine::ports::{
        ClockPort, CommittedGridWrite, EffectStatus, EffectStatusUpdate, OrderReceipt,
        OrderRequest, OrderStatus, PersistedGridEffect, StateRepositoryPort,
    };
    use grid_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder,
    };
    use grid_engine::snapshot::GridRuntimeSnapshot;
    use grid_engine::transition::GridEffect;
    use tokio::sync::Notify;
    use tokio::time::timeout;

    use crate::notifications::GridInternalNotification;

    use super::{GridWriteService, StartupSyncMode};

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
                recovery_anomaly_active: false,
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
                recovery_anomaly_active: false,
            }
        );
    }

    #[test]
    fn startup_sync_mode_explicitly_controls_follow_up_reconcile() {
        assert!(!StartupSyncMode::RecoverOnly.allows_follow_up_reconcile());
        assert!(StartupSyncMode::RecoverAndReconcile.allows_follow_up_reconcile());
    }

    #[tokio::test]
    async fn sync_exchange_state_emits_recovery_anomaly_flag_when_attention_is_required() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

        service
            .sync_exchange_state(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                vec![OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "unexpected-live".into(),
                    side: grid_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: grid_engine::ports::OrderStatus::New,
                }],
            )
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new("btc-core"),
                recovery_anomaly_active: true,
            }
        );
    }

    #[tokio::test]
    async fn sync_exchange_state_does_not_read_global_pending_effect_list_for_submit_hints() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        service
            .sync_exchange_state(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                Vec::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            repository.global_pending_effect_queries(),
            0,
            "startup sync should read grid-scoped pending submit hints instead of the global pending effect list",
        );
        assert_eq!(
            repository.pending_submit_hint_queries(),
            vec!["btc-core".to_string()]
        );
    }

    #[tokio::test]
    async fn mutations_for_same_grid_remain_serialized() {
        let repository = Arc::new(MemoryRepository::default());
        repository.block_next_save("btc-core");
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let first_service = service.clone();
        let first_mutation =
            tokio::spawn(async move { first_service.observe_market("btc-core", 95.0).await });
        repository.wait_for_save_started("btc-core", 1).await;

        let second_service = service.clone();
        let second_mutation =
            tokio::spawn(
                async move { second_service.command("btc-core", GridCommand::Pause).await },
            );

        assert!(
            timeout(
                Duration::from_millis(100),
                repository.wait_for_save_started("btc-core", 2),
            )
            .await
            .is_err(),
            "same-grid mutation should wait for the in-flight commit"
        );

        repository.release_save("btc-core");

        first_mutation.await.unwrap().unwrap();
        second_mutation.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn mutations_for_different_grids_do_not_share_global_lock() {
        let repository = Arc::new(MemoryRepository::default());
        repository.block_next_save("btc-core");
        let service = multi_grid_service(
            repository.clone() as Arc<dyn StateRepositoryPort>,
            &[("btc-core", "BTCUSDT"), ("eth-core", "ETHUSDT")],
        );

        let first_service = service.clone();
        let first_mutation =
            tokio::spawn(async move { first_service.observe_market("btc-core", 95.0).await });
        repository.wait_for_save_started("btc-core", 1).await;

        let second_service = service.clone();
        let second_mutation =
            tokio::spawn(
                async move { second_service.command("eth-core", GridCommand::Pause).await },
            );

        let completed_before_release = timeout(
            Duration::from_millis(100),
            repository.wait_for_completed_save_count(1),
        )
        .await;
        let completed_snapshot = repository.completed_saves();

        repository.release_save("btc-core");
        first_mutation.await.unwrap().unwrap();
        second_mutation.await.unwrap().unwrap();

        assert!(
            completed_before_release.is_ok(),
            "different grids should not share a global write lock"
        );
        assert_eq!(completed_snapshot, vec!["eth-core".to_string()]);
    }

    #[tokio::test]
    async fn recover_submit_effect_uses_same_per_grid_guard_as_regular_mutations() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_grid_service(
            repository.clone() as Arc<dyn StateRepositoryPort>,
            &[("btc-core", "BTCUSDT"), ("eth-core", "ETHUSDT")],
        );
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
            reduce_only: false,
        };
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order_from_submit_request(&request, Exposure(6.0)),
            SlotState::SubmitPending,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_grid_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);

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

        repository.block_next_save("btc-core");
        let started_before_recovery = repository.save_started_count("btc-core");
        let completed_before_recovery = repository.completed_saves().len();

        let recover_service = service.clone();
        let recover_request = request.clone();
        let recover_target_exposure = Exposure(6.0);
        let recover_task = tokio::spawn(async move {
            recover_service
                .recover_submit_effect(
                    "btc-core",
                    "btc-core:recovery:0",
                    &recover_request,
                    recover_target_exposure,
                    None,
                )
                .await
        });
        let recovery_started = timeout(
            Duration::from_millis(100),
            repository.wait_for_save_started("btc-core", started_before_recovery + 1),
        )
        .await;
        if recovery_started.is_err() {
            recover_task.abort();
        }
        assert!(
            recovery_started.is_ok(),
            "recover_submit_effect should reach persistence before this test checks cross-grid isolation"
        );

        let other_grid_service = service.clone();
        let other_grid_mutation = tokio::spawn(async move {
            other_grid_service
                .command("eth-core", GridCommand::Pause)
                .await
        });

        let other_grid_completed = timeout(
            Duration::from_millis(100),
            repository.wait_for_completed_save_count(completed_before_recovery + 1),
        )
        .await;
        let completed_snapshot = repository.completed_saves();

        repository.release_save("btc-core");

        assert!(matches!(
            recover_task.await.unwrap().unwrap(),
            SubmitRecoveryResolution::Superseded { .. }
        ));
        other_grid_mutation.await.unwrap().unwrap();

        assert!(
            other_grid_completed.is_ok(),
            "other grids should remain writable while recovery is persisting"
        );
        assert_eq!(
            completed_snapshot,
            vec!["btc-core".to_string(), "eth-core".to_string()]
        );
    }

    #[tokio::test]
    async fn recovers_slot_workset_from_live_exchange_state() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        snapshot.current_exposure = Exposure(2.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "restore-1",
                Side::Buy,
                94.5,
                0.25,
                Exposure(2.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_grid_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);

        let transition = service
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
            )
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![]);

        let snapshot = repository.snapshot_for("btc-core").unwrap();
        let executor_state = snapshot.executor_state;
        assert_eq!(
            executor_state.slots,
            vec![grid_engine::runtime::ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(grid_engine::runtime::WorkingOrder {
                    order_id: Some("live-1".into()),
                    client_order_id: "restore-1".into(),
                    side: Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    target_exposure: Exposure(2.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
    }

    #[tokio::test]
    async fn complete_submit_execution_returns_error_when_executor_slot_is_missing() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository as Arc<dyn StateRepositoryPort>);

        let error = service
            .complete_submit_execution(
                "btc-core",
                "btc-core:batch:0",
                &OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                Exposure(4.0),
                &OrderReceipt {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    status: OrderStatus::New,
                },
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("submit receipt"));
    }

    #[tokio::test]
    async fn record_submit_failure_clears_submit_pending_slot_and_marks_effect_failed() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let transition = service.observe_market("btc-core", 95.0).await.unwrap();
        let request = match transition.effects.as_slice() {
            [GridEffect::SubmitOrder { request, .. }] => request.clone(),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        let effect_id = repository.pending_effects()[0].effect_id.clone();

        service
            .record_submit_failure(
                "btc-core",
                &effect_id,
                &request.client_order_id,
                "submit order rejected",
            )
            .await
            .unwrap();

        assert!(
            repository
                .snapshot_for("btc-core")
                .map(|snapshot| {
                    snapshot.executor_state.slots
                        == vec![ExecutionSlot {
                            slot: OrderSlot::new("inventory_core"),
                            state: SlotState::Empty,
                            working_order: None,
                        }]
                })
                .unwrap_or(false)
        );
        let effect = repository
            .all_effects()
            .into_iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("effect should remain persisted");
        assert_eq!(effect.status, EffectStatus::Failed);
        assert_eq!(effect.last_error.as_deref(), Some("submit order rejected"));
    }

    #[tokio::test]
    async fn complete_effect_failed_returns_invariant_violation_when_grid_is_not_loaded() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_grid_service(repository as Arc<dyn StateRepositoryPort>, &[]);

        let error = service
            .complete_effect_failed("btc-core", "btc-core:batch:0", "submit order rejected")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("loaded-grid invariant violated"));
        assert!(error.to_string().contains("btc-core"));
        assert!(!error.to_string().contains("grid `btc-core` not found"));
    }

    #[tokio::test]
    async fn recover_submit_effect_returns_invariant_violation_when_grid_is_not_loaded() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_grid_service(repository as Arc<dyn StateRepositoryPort>, &[]);

        let error = match service
            .recover_submit_effect(
                "btc-core",
                "btc-core:batch:0",
                &OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                Exposure(4.0),
                None,
            )
            .await
        {
            Ok(_) => panic!("submit recovery should fail when grid is not loaded"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("loaded-grid invariant violated"));
        assert!(error.to_string().contains("btc-core"));
        assert!(!error.to_string().contains("grid `btc-core` not found"));
    }

    fn working_order(
        order_id: Option<&str>,
        client_order_id: &str,
        side: Side,
        price: f64,
        quantity: f64,
        target_exposure: Exposure,
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
                Side::Buy => OrderRole::IncreaseInventory,
                Side::Sell => OrderRole::DecreaseInventory,
            },
        }
    }

    fn working_order_from_submit_request(
        request: &OrderRequest,
        target_exposure: Exposure,
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

    fn set_executor_state(
        snapshot: &mut GridRuntimeSnapshot,
        order: WorkingOrder,
        state: SlotState,
    ) {
        snapshot.executor_state = ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: snapshot.current_exposure.delta(&order.target_exposure),
            gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state,
                working_order: Some(order),
            }],
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                max_inventory_gap_abs: Exposure(0.0),
                max_gap_age_ms: 0,
            },
        };
    }

    #[tokio::test]
    async fn recover_submit_effect_proceed_persists_submit_pending_slot_before_returning() {
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

        assert!(matches!(
            resolution,
            SubmitRecoveryResolution::Proceed { .. }
        ));
        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .map(|snapshot| snapshot.executor_state)
                .and_then(|state| state.slots.into_iter().next())
                .and_then(|slot| slot.working_order),
            Some(working_order_from_submit_request(
                &request,
                target_exposure.clone()
            ))
        );
        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        assert_eq!(
            manager
                .snapshot("btc-core")
                .map(|snapshot| snapshot.executor_state)
                .and_then(|state| state.slots.into_iter().next())
                .and_then(|slot| slot.working_order),
            Some(working_order_from_submit_request(&request, target_exposure))
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
            reduce_only: false,
        };
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order_from_submit_request(&request, Exposure(6.0)),
            SlotState::SubmitPending,
        );
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

        let SubmitRecoveryResolution::Superseded { state } = resolution else {
            panic!("expected stale submit effect to be superseded");
        };
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
            } => Some(working_order_from_submit_request(
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
            repository
                .snapshot_for("btc-core")
                .map(|snapshot| snapshot.executor_state)
                .and_then(|state| state.slots.into_iter().next())
                .and_then(|slot| slot.working_order),
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
        multi_grid_service(repository, &[("btc-core", "BTCUSDT")])
    }

    fn multi_grid_service(
        repository: Arc<dyn StateRepositoryPort>,
        grids: &[(&str, &str)],
    ) -> GridWriteService {
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let mut manager = GridManager::new(Arc::new(FixedClock));
        for (id, symbol) in grids {
            manager
                .add_grid(
                    GridId::new(*id),
                    Instrument::new(Venue::Binance, *symbol),
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
                        maker_fee_rate: 0.0,
                        taker_fee_rate: 0.0,
                    },
                )
                .unwrap();
        }

        GridWriteService::new(manager, repository, notifications)
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: Mutex<HashMap<String, GridRuntimeSnapshot>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedGridEffect>>,
        next_effect_seq: Mutex<u64>,
        global_pending_effect_queries: AtomicUsize,
        pending_submit_hint_queries: Mutex<Vec<String>>,
        save_controls: Mutex<HashMap<String, Arc<SaveControl>>>,
        completed_saves: Mutex<Vec<String>>,
        completed_notify: Notify,
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

        fn global_pending_effect_queries(&self) -> usize {
            self.global_pending_effect_queries.load(Ordering::SeqCst)
        }

        fn pending_submit_hint_queries(&self) -> Vec<String> {
            self.pending_submit_hint_queries.lock().unwrap().clone()
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

        fn block_next_save(&self, id: &str) {
            self.save_control(id)
                .block_next
                .store(true, Ordering::SeqCst);
        }

        async fn wait_for_save_started(&self, id: &str, expected_count: usize) {
            let control = self.save_control(id);
            while control.started.load(Ordering::SeqCst) < expected_count {
                control.started_notify.notified().await;
            }
        }

        fn save_started_count(&self, id: &str) -> usize {
            self.save_control(id).started.load(Ordering::SeqCst)
        }

        fn release_save(&self, id: &str) {
            self.save_control(id).release_notify.notify_one();
        }

        fn completed_saves(&self) -> Vec<String> {
            self.completed_saves.lock().unwrap().clone()
        }

        async fn wait_for_completed_save_count(&self, expected_count: usize) {
            while self.completed_saves.lock().unwrap().len() < expected_count {
                self.completed_notify.notified().await;
            }
        }

        fn save_control(&self, id: &str) -> Arc<SaveControl> {
            let mut controls = self.save_controls.lock().unwrap();
            Arc::clone(
                controls
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::new(SaveControl::default())),
            )
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
            let save_control = self.save_control(id);
            save_control.started.fetch_add(1, Ordering::SeqCst);
            save_control.started_notify.notify_waiters();
            if save_control.block_next.swap(false, Ordering::SeqCst) {
                save_control.release_notify.notified().await;
            }

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

            self.completed_saves.lock().unwrap().push(id.to_string());
            self.completed_notify.notify_waiters();

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

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            self.global_pending_effect_queries
                .fetch_add(1, Ordering::SeqCst);
            Ok(self.pending_effects())
        }

        async fn list_pending_submit_effects_for_grid(
            &self,
            grid_id: &GridId,
        ) -> Result<Vec<PersistedGridEffect>> {
            self.pending_submit_hint_queries
                .lock()
                .unwrap()
                .push(grid_id.as_str().to_string());
            Ok(self
                .pending_effects()
                .into_iter()
                .filter(|effect| effect.grid_id == *grid_id)
                .filter(|effect| matches!(effect.effect, GridEffect::SubmitOrder { .. }))
                .collect())
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

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(Vec::new())
        }

        async fn list_pending_submit_effects_for_grid(
            &self,
            _grid_id: &GridId,
        ) -> Result<Vec<PersistedGridEffect>> {
            Ok(Vec::new())
        }
    }

    struct FixedClock;

    #[derive(Default)]
    struct SaveControl {
        block_next: AtomicBool,
        started: AtomicUsize,
        started_notify: Notify,
        release_notify: Notify,
    }

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
