use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    ApplicationNotification, EffectStatus, EffectStatusUpdate, FollowUpRetirementRequest,
    PersistedTrackEffect, TrackEffectStore, TrackMutationStore,
};
use anyhow::{Result, anyhow};
use poise_core::events::DomainEvent;
use poise_engine::command::TrackCommand;
use poise_engine::executor::{
    OrderSlot, OrderUpdateAbsorbResult, SubmitRecoveryPlan, SubmitRecoveryResolution,
};
use poise_engine::ledger::TrackLedgerEvent;
use poise_engine::manager::MarketMutationOutcome;
use poise_engine::manager::{ExchangeSyncMode, TrackManager};
use poise_engine::observation::{
    MarketObservation, OrderObservation, PositionObservation, TrackObservation,
};
use poise_engine::ports::{ExchangeOrder, OrderReceipt, OrderRequest};
use poise_engine::runtime::AccountCapacityConstraint;
use poise_engine::track::{Instrument, TrackId};
use poise_engine::transition::{TrackEffect, TrackTransition};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast};

use crate::submit_effect_service::{SubmitAttemptResult, SubmitExecutionRecovery};

pub struct TrackServiceSet {
    pub command: crate::TrackCommandService,
    pub observation: crate::TrackObservationService,
    pub effect: crate::TrackEffectService,
    pub submit_effect: crate::submit_effect_service::SubmitEffectService,
}

pub trait AccountCapacityGuard: Send + Sync {
    fn constraint_for(&self, instrument: &Instrument) -> AccountCapacityConstraint;
}

pub trait RecoveryAnomalyObserver: Send + Sync {
    fn observe_recovery_anomaly_change(&self, track_id: &TrackId, active: bool);
}

struct NoopRecoveryAnomalyObserver;

impl RecoveryAnomalyObserver for NoopRecoveryAnomalyObserver {
    fn observe_recovery_anomaly_change(&self, _track_id: &TrackId, _active: bool) {}
}

type SharedManager = Arc<RwLock<TrackManager>>;

#[derive(Default)]
struct TrackMutationGuards {
    locks: Mutex<HashMap<TrackId, Arc<Mutex<()>>>>,
}

impl TrackMutationGuards {
    async fn lock(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(
                locks
                    .entry(TrackId::new(id))
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };

        lock.lock_owned().await
    }
}

#[derive(Clone)]
pub(crate) struct MutationExecutor {
    manager: SharedManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    mutation_guards: Arc<TrackMutationGuards>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<dyn AccountCapacityGuard>,
    recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInstrument {
    pub id: String,
    pub instrument: Instrument,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplyTrackLedgerEventResult {
    pub absorb_result: Option<OrderUpdateAbsorbResult>,
    pub order_status: Option<poise_engine::ports::OrderStatus>,
    pub domain_events: Vec<DomainEvent>,
    pub effects: Vec<TrackEffect>,
}

#[derive(Debug)]
pub enum TrackMutationError {
    LoadedTrackInvariant { track_id: String },
    Mutation(anyhow::Error),
    Persistence(anyhow::Error),
}

impl std::fmt::Display for TrackMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for TrackMutationError {}

impl TrackMutationError {
    pub fn message(&self) -> String {
        match self {
            Self::LoadedTrackInvariant { track_id } => format!(
                "loaded-track invariant violated for effect writeback: track `{track_id}` is not loaded in write-side runtime"
            ),
            Self::Mutation(error) | Self::Persistence(error) => error.to_string(),
        }
    }

    fn loaded_track_invariant(track_id: &str) -> Self {
        Self::LoadedTrackInvariant {
            track_id: track_id.to_string(),
        }
    }

    fn is_loaded_track_invariant_violation(&self) -> bool {
        matches!(self, Self::LoadedTrackInvariant { .. })
    }
}

pub fn is_loaded_track_invariant_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<TrackMutationError>()
        .is_some_and(TrackMutationError::is_loaded_track_invariant_violation)
}

pub(crate) trait TransitionResult {
    fn domain_events(&self) -> &[DomainEvent];
    fn effects(&self) -> &[TrackEffect];
}

impl TransitionResult for () {
    fn domain_events(&self) -> &[DomainEvent] {
        &[]
    }

    fn effects(&self) -> &[TrackEffect] {
        &[]
    }
}

impl TransitionResult for TrackTransition {
    fn domain_events(&self) -> &[DomainEvent] {
        &self.events
    }

    fn effects(&self) -> &[TrackEffect] {
        &self.effects
    }
}

impl TransitionResult for (TrackTransition, OrderUpdateAbsorbResult) {
    fn domain_events(&self) -> &[DomainEvent] {
        self.0.domain_events()
    }

    fn effects(&self) -> &[TrackEffect] {
        self.0.effects()
    }
}

impl TransitionResult for SubmitRecoveryPlan {
    fn domain_events(&self) -> &[DomainEvent] {
        &[]
    }

    fn effects(&self) -> &[TrackEffect] {
        &self.effects
    }
}

impl TransitionResult for ApplyTrackLedgerEventResult {
    fn domain_events(&self) -> &[DomainEvent] {
        &self.domain_events
    }

    fn effects(&self) -> &[TrackEffect] {
        &self.effects
    }
}

impl MutationExecutor {
    pub(crate) fn new(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_store: Arc<dyn TrackEffectStore>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        Self {
            manager: Arc::new(RwLock::new(manager)),
            mutation_store,
            effect_store,
            mutation_guards: Arc::new(TrackMutationGuards::default()),
            notifications,
            account_margin_guard,
            recovery_anomaly_observer,
        }
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub(crate) fn manager(&self) -> SharedManager {
        Arc::clone(&self.manager)
    }

    pub(crate) fn emit_internal_notification(&self, notification: ApplicationNotification) {
        let _ = self.notifications.send(notification);
    }

    pub(crate) async fn has_track(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_track(id).is_some()
    }

    pub(crate) async fn track_instruments(&self) -> Vec<TrackInstrument> {
        let manager = self.manager.read().await;
        manager
            .list_tracks()
            .into_iter()
            .map(|track| TrackInstrument {
                id: track.id().as_str().to_string(),
                instrument: track.instrument().clone(),
            })
            .collect()
    }

    pub(crate) async fn resolve_track_id(&self, instrument: &Instrument) -> Option<String> {
        let manager = self.manager.read().await;
        manager
            .resolve_track_id(instrument)
            .map(|track_id| track_id.as_str().to_string())
    }
}

impl TrackServiceSet {
    pub fn new(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_store: Arc<dyn TrackEffectStore>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
    ) -> Self {
        Self::new_with_recovery_anomaly_observer(
            manager,
            mutation_store,
            effect_store,
            notifications,
            account_margin_guard,
            Arc::new(NoopRecoveryAnomalyObserver),
        )
    }

    pub fn new_with_recovery_anomaly_observer(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_store: Arc<dyn TrackEffectStore>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        let executor = Arc::new(MutationExecutor::new(
            manager,
            mutation_store,
            effect_store,
            notifications,
            account_margin_guard,
            recovery_anomaly_observer,
        ));
        Self {
            command: crate::TrackCommandService::from_executor(executor.clone()),
            observation: crate::TrackObservationService::from_executor(executor.clone()),
            effect: crate::TrackEffectService::from_executor(executor.clone()),
            submit_effect: crate::submit_effect_service::SubmitEffectService::from_executor(
                executor,
            ),
        }
    }
}
impl MutationExecutor {
    pub(crate) async fn observe_market(
        &self,
        id: &str,
        observation: MarketObservation,
    ) -> Result<TrackTransition> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let previous_snapshot = {
            let manager = self.manager.read().await;
            manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?
        };

        let outcome = self
            .with_manager_mutation(id, |manager| {
                manager.observe_market_mutation(&TrackId::new(id), observation.clone())
            })
            .await
            .map_err(anyhow::Error::new)?;

        match outcome {
            MarketMutationOutcome::LiveOnly => {
                let next_snapshot = {
                    let manager = self.manager.read().await;
                    manager
                        .snapshot(id)
                        .ok_or_else(|| anyhow!("track `{id}` not found"))?
                };

                if next_snapshot != previous_snapshot {
                    self.commit_track_mutation(
                        id,
                        &previous_snapshot,
                        &next_snapshot,
                        &(),
                        None,
                        false,
                    )
                    .await
                    .map_err(anyhow::Error::new)?;
                }

                Ok(TrackTransition {
                    snapshot: next_snapshot,
                    events: Vec::new(),
                    effects: Vec::new(),
                })
            }
            MarketMutationOutcome::Durable(transition) => {
                self.commit_track_mutation(
                    id,
                    &previous_snapshot,
                    &transition.snapshot,
                    &transition,
                    None,
                    false,
                )
                .await
                .map_err(anyhow::Error::new)?;
                Ok(transition)
            }
        }
    }

    pub async fn command(&self, id: &str, command: TrackCommand) -> Result<TrackTransition> {
        self.mutate_track(id, |manager| {
            manager.command(&TrackId::new(id), command.clone())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn refresh_market_data_health(&self, id: &str) -> Result<TrackTransition> {
        self.mutate_track_skip_noop(id, |manager| {
            manager.refresh_market_data_health(&TrackId::new(id))
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub(crate) async fn market_data_health_deadline(
        &self,
        id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let manager = self.manager.read().await;
        manager.market_data_health_deadline(&TrackId::new(id))
    }

    pub async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<TrackTransition> {
        self.mutate_track(id, |manager| {
            manager.observe(
                &TrackId::new(id),
                TrackObservation::Position(observation.clone()),
            )
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, OrderUpdateAbsorbResult)> {
        let result = self
            .mutate_track(id, |manager| {
                manager.observe_order_update(&TrackId::new(id), observation.clone())
            })
            .await
            .map_err(anyhow::Error::new)?;

        if observation.status.clears_working_order()
            && result.1 != OrderUpdateAbsorbResult::Unabsorbed
        {
            self.retry_pending_follow_up_retirements_best_effort(
                id,
                "terminal order observation writeback",
            )
            .await;
        }

        Ok(result)
    }

    pub async fn apply_track_ledger_event(
        &self,
        id: &str,
        event: TrackLedgerEvent,
    ) -> Result<ApplyTrackLedgerEventResult> {
        self.mutate_track(id, |manager| match &event {
            TrackLedgerEvent::Execution(update) => {
                let mut order_update = update.order_update.clone();
                order_update.realized_pnl = 0.0;
                let (transition, absorb_result) =
                    manager.observe_order_update(&TrackId::new(id), order_update)?;
                manager.apply_ledger_adjustment(
                    &TrackId::new(id),
                    &update.ledger_deltas,
                    &update.ledger_gaps,
                )?;
                Ok(ApplyTrackLedgerEventResult {
                    absorb_result: Some(absorb_result),
                    order_status: Some(update.order_update.status),
                    domain_events: transition.events,
                    effects: transition.effects,
                })
            }
            TrackLedgerEvent::Adjustment(update) => {
                manager.apply_ledger_adjustment(
                    &TrackId::new(id),
                    &update.ledger_deltas,
                    &update.ledger_gaps,
                )?;
                Ok(ApplyTrackLedgerEventResult {
                    absorb_result: None,
                    order_status: None,
                    domain_events: Vec::new(),
                    effects: Vec::new(),
                })
            }
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn sync_exchange_state(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
    ) -> Result<TrackTransition> {
        self.sync_exchange_state_inner(
            id,
            position,
            open_orders,
            ExchangeSyncMode::RecoverAndReconcile,
        )
        .await
    }

    pub async fn sync_exchange_state_without_reconcile(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
    ) -> Result<TrackTransition> {
        self.sync_exchange_state_inner(id, position, open_orders, ExchangeSyncMode::RecoverOnly)
            .await
    }

    async fn sync_exchange_state_inner(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        mode: ExchangeSyncMode,
    ) -> Result<TrackTransition> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let pending_submit_hints = self
            .effect_store
            .list_pending_submit_effects_for_track(&TrackId::new(id))
            .await
            .map_err(TrackMutationError::Persistence)?
            .into_iter()
            .filter_map(|effect| match effect.effect {
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                    submit_purpose,
                } => Some(poise_engine::executor::PendingSubmitHint {
                    request,
                    desired_exposure,
                    submit_purpose,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (previous_snapshot, transition, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            self.sync_account_capacity_constraint(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let transition = if mode.allows_follow_up_reconcile() {
                manager
                    .sync_exchange_state(
                        &TrackId::new(id),
                        position,
                        open_orders,
                        pending_submit_hints,
                    )
                    .map_err(|error| {
                        manager
                            .restore_track_state(&previous_snapshot)
                            .expect("failed to restore previous snapshot after sync_exchange_state mutation error");
                        TrackMutationError::Mutation(error)
                    })?
            } else {
                manager
                    .sync_exchange_state_without_reconcile(
                        &TrackId::new(id),
                        position,
                        open_orders,
                        pending_submit_hints,
                    )
                    .map_err(|error| {
                        manager
                            .restore_track_state(&previous_snapshot)
                            .expect("failed to restore previous snapshot after sync_exchange_state_without_reconcile mutation error");
                        TrackMutationError::Mutation(error)
                    })?
            };
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            (previous_snapshot, transition, next_snapshot)
        };

        self.commit_track_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &transition,
            None,
            false,
        )
        .await
        .map_err(anyhow::Error::new)?;

        drop(_mutation_guard);
        self.retry_pending_follow_up_retirements_best_effort(id, "exchange state sync writeback")
            .await;

        Ok(transition)
    }

    pub(crate) async fn complete_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<SubmitAttemptResult> {
        self.mutate_track_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |manager| {
                manager.record_submit_receipt(
                    &TrackId::new(id),
                    request,
                    desired_exposure.clone(),
                    receipt,
                )?;
                Ok(())
            },
        )
        .await
        .map(|_| SubmitAttemptResult::changed())
        .map_err(anyhow::Error::new)
    }

    pub(crate) async fn record_submit_failure(
        &self,
        id: &str,
        effect_id: &str,
        client_order_id: &str,
        error: &str,
    ) -> Result<SubmitAttemptResult> {
        self.mutate_track_with_effect_status(
            id,
            effect_status_failed(effect_id, error),
            |manager| {
                manager.record_submit_failure(&TrackId::new(id), client_order_id)?;
                Ok(())
            },
        )
        .await
        .map(|_| SubmitAttemptResult::changed())
        .map_err(anyhow::Error::new)
    }

    pub(crate) async fn complete_submit_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<SubmitAttemptResult> {
        self.complete_effect_failed(id, effect_id, error).await?;
        Ok(SubmitAttemptResult::changed())
    }

    pub async fn record_cancel_order_success(
        &self,
        id: &str,
        effect_id: &str,
        batch_id: &str,
        sequence: u32,
        order_id: &str,
    ) -> Result<()> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let replacement_submit = self
            .pending_submit_effects_for_track_batch(id, batch_id)
            .await
            .map_err(anyhow::Error::new)
            .and_then(|effects| select_replacement_submit_effect(&effects, sequence))?;
        let effect_status_update = EffectStatusUpdate::succeeded(effect_id.to_string());
        let (previous_snapshot, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            manager
                .record_cancel_order_success(&TrackId::new(id), order_id)
                .map_err(TrackMutationError::Mutation)?;
            if let Some(replacement_submit) = replacement_submit.as_ref() {
                restore_ready_pending_submit_effect(&mut manager, id, replacement_submit)
                    .map_err(TrackMutationError::Mutation)?;
            }
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_snapshot, next_snapshot)
        };

        self.commit_track_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &(),
            Some(&effect_status_update),
            false,
        )
        .await
        .map_err(anyhow::Error::new)?;

        drop(_mutation_guard);
        self.retry_pending_follow_up_retirements_best_effort(id, "cancel success writeback")
            .await;

        Ok(())
    }

    pub async fn record_cancel_all_success(&self, id: &str, effect_id: &str) -> Result<()> {
        self.mutate_track_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |manager| {
                manager.record_cancel_all_success(&TrackId::new(id))?;
                Ok(())
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.mutate_track_with_effect_status(
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
        self.mutate_track_with_effect_status(
            id,
            effect_status_failed(effect_id, error),
            |_manager| Ok(()),
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    pub async fn retire_stale_follow_up_submit(
        &self,
        id: &str,
        request: &FollowUpRetirementRequest,
    ) -> Result<bool> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let replacement_submit = self
            .pending_submit_effects_for_track_batch(id, &request.batch_id)
            .await
            .map_err(anyhow::Error::new)
            .and_then(|effects| {
                select_replacement_submit_effect(&effects, request.blocked_sequence)
            })?;
        let Some(replacement_submit) = replacement_submit else {
            return Ok(true);
        };

        let TrackEffect::SubmitOrder {
            request: submit_request,
            ..
        } = &replacement_submit.effect
        else {
            return Err(anyhow!(
                "replacement effect `{}` is not submit order",
                replacement_submit.effect_id
            ));
        };

        let (previous_snapshot, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            let lifecycle_closed = previous_snapshot.executor_state.slots.iter().all(|slot| {
                slot.working_order
                    .as_ref()
                    .and_then(|order| order.order_id.as_deref())
                    != Some(request.closed_order_id.as_str())
            });
            if !lifecycle_closed {
                return Ok(false);
            }
            manager
                .record_submit_failure(&TrackId::new(id), &submit_request.client_order_id)
                .map_err(TrackMutationError::Mutation)?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_snapshot, next_snapshot)
        };

        self.commit_track_mutation(
            id,
            &previous_snapshot,
            &next_snapshot,
            &(),
            Some(&EffectStatusUpdate::superseded(
                replacement_submit.effect_id.clone(),
            )),
            false,
        )
        .await
        .map_err(anyhow::Error::new)?;

        Ok(true)
    }

    pub async fn request_follow_up_retirement(
        &self,
        id: &str,
        request: FollowUpRetirementRequest,
    ) -> Result<()> {
        self.effect_store
            .save_follow_up_retirement_request(&TrackId::new(id), &request)
            .await?;
        self.retry_pending_follow_up_retirements(id).await?;
        Ok(())
    }

    pub(crate) async fn retry_pending_follow_up_retirements_best_effort(
        &self,
        id: &str,
        context: &str,
    ) {
        if let Err(error) = self.retry_pending_follow_up_retirements(id).await {
            tracing::warn!(
                track_id = %id,
                "failed to retry pending follow-up retirements after {context}: {error}"
            );
        }
    }

    async fn retry_pending_follow_up_retirements(&self, id: &str) -> Result<()> {
        for request in self
            .effect_store
            .list_follow_up_retirement_requests(&TrackId::new(id))
            .await?
        {
            if self.retire_stale_follow_up_submit(id, &request).await? {
                self.effect_store
                    .delete_follow_up_retirement_request(&TrackId::new(id), &request)
                    .await?;
            }
        }

        Ok(())
    }

    pub(crate) async fn recover_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitExecutionRecovery> {
        Ok(
            match self
                .recover_submit_effect(id, effect_id, request, desired_exposure, live_order)
                .await?
            {
                SubmitRecoveryResolution::Proceed {
                    desired_exposure, ..
                } => SubmitExecutionRecovery::Dispatch { desired_exposure },
                SubmitRecoveryResolution::Recovered { .. } => {
                    SubmitExecutionRecovery::Finished(SubmitAttemptResult::changed())
                }
                SubmitRecoveryResolution::Superseded { .. } => {
                    SubmitExecutionRecovery::Finished(SubmitAttemptResult::changed())
                }
                SubmitRecoveryResolution::AwaitExchangeState => {
                    SubmitExecutionRecovery::Finished(SubmitAttemptResult::unchanged())
                }
            },
        )
    }

    async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryResolution> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let (previous_snapshot, plan, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            self.sync_account_capacity_constraint(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let plan = manager
                .recover_submit_effect(
                    &TrackId::new(id),
                    request,
                    desired_exposure.clone(),
                    live_order,
                )
                .map_err(|error| {
                    manager
                        .restore_track_state(&previous_snapshot)
                        .expect("failed to restore previous snapshot after recover_submit_effect mutation error");
                    TrackMutationError::Mutation(error)
                })?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
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

        self.commit_track_mutation(
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

    async fn mutate_track_with_effect_status<R, F>(
        &self,
        id: &str,
        effect_status_update: EffectStatusUpdate,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let (previous_snapshot, result, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            self.sync_account_capacity_constraint(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let result = mutate(&mut manager).map_err(|error| {
                manager
                    .restore_track_state(&previous_snapshot)
                    .expect("failed to restore previous snapshot after mutation error");
                TrackMutationError::Mutation(error)
            })?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_snapshot, result, next_snapshot)
        };

        self.commit_track_mutation(
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

    async fn mutate_track<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
        R: TransitionResult,
    {
        self.mutate_track_with_options(id, false, mutate).await
    }

    async fn mutate_track_skip_noop<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
        R: TransitionResult,
    {
        self.mutate_track_with_options(id, true, mutate).await
    }

    async fn mutate_track_with_options<R, F>(
        &self,
        id: &str,
        skip_when_noop: bool,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
        R: TransitionResult,
    {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let (previous_snapshot, result, next_snapshot) = {
            let mut manager = self.manager.write().await;
            let previous_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            self.sync_account_capacity_constraint(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let result = mutate(&mut manager).map_err(|error| {
                manager
                    .restore_track_state(&previous_snapshot)
                    .expect("failed to restore previous snapshot after mutation error");
                TrackMutationError::Mutation(error)
            })?;
            let next_snapshot = manager
                .snapshot(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            (previous_snapshot, result, next_snapshot)
        };

        self.commit_track_mutation(
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

    async fn with_manager_mutation<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
    {
        let mut manager = self.manager.write().await;
        let previous_snapshot = manager
            .snapshot(id)
            .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
        self.sync_account_capacity_constraint(&mut manager, id)
            .map_err(TrackMutationError::Mutation)?;
        mutate(&mut manager).map_err(|error| {
            manager
                .restore_track_state(&previous_snapshot)
                .expect("failed to restore previous snapshot after mutation error");
            TrackMutationError::Mutation(error)
        })
    }

    async fn lock_track_mutation(&self, id: &str) -> OwnedMutexGuard<()> {
        self.mutation_guards.lock(id).await
    }

    fn sync_account_capacity_constraint(&self, manager: &mut TrackManager, id: &str) -> Result<()> {
        let Some(mut snapshot) = manager.snapshot(id) else {
            return Ok(());
        };
        let Some(track) = manager.get_track(id) else {
            return Ok(());
        };
        let constraint = self.account_margin_guard.constraint_for(track.instrument());
        if snapshot.risk.account_capacity_constraint == constraint {
            return Ok(());
        }
        snapshot.risk.account_capacity_constraint = AccountCapacityConstraint {
            increase_blocked: constraint.increase_blocked,
            blocked_reason: constraint.blocked_reason,
            max_increase_notional: constraint.max_increase_notional,
        };
        manager.restore_track_state(&snapshot)
    }

    async fn pending_submit_effects_for_track_batch(
        &self,
        id: &str,
        batch_id: &str,
    ) -> std::result::Result<Vec<PersistedTrackEffect>, TrackMutationError> {
        self.effect_store
            .list_pending_submit_effects_for_track_batch(&TrackId::new(id), batch_id)
            .await
            .map_err(TrackMutationError::Persistence)
    }

    async fn commit_track_mutation<R>(
        &self,
        id: &str,
        previous_snapshot: &poise_engine::snapshot::TrackRuntimeSnapshot,
        next_snapshot: &poise_engine::snapshot::TrackRuntimeSnapshot,
        result: &R,
        effect_status_update: Option<&EffectStatusUpdate>,
        skip_when_noop: bool,
    ) -> std::result::Result<(), TrackMutationError>
    where
        R: TransitionResult,
    {
        let has_track_write = previous_snapshot != next_snapshot
            || !result.domain_events().is_empty()
            || !result.effects().is_empty();
        let has_effect_status_update = effect_status_update.is_some();
        let has_persistence_work = has_track_write || has_effect_status_update;
        if skip_when_noop && !has_persistence_work {
            return Ok(());
        }

        if let Err(error) = self
            .mutation_store
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
                manager.restore_track_state(previous_snapshot)
            };
            if let Err(rollback_error) = rollback_result {
                return Err(TrackMutationError::Persistence(anyhow!(
                    "failed to persist track `{id}`: {error}; rollback failed: {rollback_error}"
                )));
            }
            return Err(TrackMutationError::Persistence(error));
        }

        let previous_recovery_anomaly_active = previous_snapshot
            .executor_state
            .diagnostics
            .recovery_anomaly
            .is_some();
        let next_recovery_anomaly_active = next_snapshot
            .executor_state
            .diagnostics
            .recovery_anomaly
            .is_some();
        if previous_recovery_anomaly_active != next_recovery_anomaly_active {
            self.recovery_anomaly_observer
                .observe_recovery_anomaly_change(&TrackId::new(id), next_recovery_anomaly_active);
        }

        if has_track_write || has_effect_status_update {
            self.emit_internal_notification(ApplicationNotification::TrackChanged {
                track_id: TrackId::new(id),
            });
        }

        Ok(())
    }
}

fn effect_status_failed(effect_id: &str, error: &str) -> EffectStatusUpdate {
    EffectStatusUpdate {
        effect_id: effect_id.to_string(),
        status: EffectStatus::Failed,
        attempt_delta: 1,
        last_error: Some(error.to_string()),
    }
}

fn restore_ready_pending_submit_effect(
    manager: &mut TrackManager,
    id: &str,
    replacement_submit: &PersistedTrackEffect,
) -> Result<()> {
    let snapshot = manager
        .snapshot(id)
        .ok_or_else(|| anyhow!("track `{id}` not found"))?;
    let inventory_core_has_working_order = snapshot
        .executor_state
        .slots
        .iter()
        .find(|slot| slot.slot == OrderSlot::new("inventory_core"))
        .and_then(|slot| slot.working_order.as_ref())
        .is_some();
    if inventory_core_has_working_order {
        return Ok(());
    }

    let TrackEffect::SubmitOrder {
        request,
        desired_exposure,
        ..
    } = &replacement_submit.effect
    else {
        return Err(anyhow!(
            "replacement effect `{}` is not submit order",
            replacement_submit.effect_id
        ));
    };

    manager.record_submit_request(&TrackId::new(id), request, desired_exposure.clone())?;
    Ok(())
}

fn select_replacement_submit_effect(
    effects: &[PersistedTrackEffect],
    after_sequence: u32,
) -> Result<Option<PersistedTrackEffect>> {
    let matching = effects
        .iter()
        .filter(|effect| effect.sequence > after_sequence)
        .cloned()
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(None),
        [effect] => Ok(Some(effect.clone())),
        _ => Err(anyhow!(
            "multiple replacement submit effects found after sequence {after_sequence}"
        )),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use poise_core::events::DomainEvent;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{ClockPort, OrderRequest};
    use poise_engine::snapshot::TrackRuntimeSnapshot;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use tokio::sync::broadcast;

    use crate::{
        ApplicationNotification, CommittedTrackWrite, EffectStatus, EffectStatusUpdate,
        FollowUpRetirementRequest, PersistedTrackEffect, TrackEffectStore, TrackMutationStore,
    };

    use super::{AccountCapacityGuard, TrackServiceSet};

    #[derive(Default)]
    pub(crate) struct NoopGuard;

    impl AccountCapacityGuard for NoopGuard {
        fn constraint_for(
            &self,
            _instrument: &Instrument,
        ) -> poise_engine::runtime::AccountCapacityConstraint {
            poise_engine::runtime::AccountCapacityConstraint::default()
        }
    }

    pub(crate) struct TestClock;

    impl ClockPort for TestClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now()
        }
    }

    #[derive(Default)]
    pub(crate) struct MemoryRepository {
        snapshots: Mutex<HashMap<String, TrackRuntimeSnapshot>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedTrackEffect>>,
        retirement_requests: Mutex<HashMap<String, Vec<FollowUpRetirementRequest>>>,
    }

    impl MemoryRepository {
        pub(crate) fn pending_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().unwrap().clone()
        }

        pub(crate) fn seed_pending_submit_effect(&self) {
            self.effects.lock().unwrap().push(PersistedTrackEffect {
                effect_id: "btc-core:batch-1:0".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "btc-core:batch-1".into(),
                sequence: 0,
                effect: TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        side: Side::Buy,
                        price: 100.0,
                        quantity: 0.1,
                        client_order_id: "client-1".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
        }
    }

    #[async_trait]
    impl TrackMutationStore for MemoryRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &TrackRuntimeSnapshot,
            events: &[DomainEvent],
            effects: &[TrackEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            self.snapshots
                .lock()
                .unwrap()
                .insert(id.to_string(), state.clone());
            self.events
                .lock()
                .unwrap()
                .insert(id.to_string(), events.to_vec());

            let now = Utc::now();
            let mut stored_effects = self.effects.lock().unwrap();
            let mut committed_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                let batch_id = format!("{id}:batch");
                let effect_id = format!("{batch_id}:{sequence}");
                let persisted = PersistedTrackEffect {
                    effect_id: effect_id.clone(),
                    track_id: TrackId::new(id),
                    batch_id,
                    sequence: sequence as u32,
                    effect: effect.clone(),
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                };
                stored_effects.push(persisted.clone());
                committed_effects.push(persisted);
            }

            if let Some(update) = effect_status_update
                && let Some(effect) = stored_effects
                    .iter_mut()
                    .find(|effect| effect.effect_id == update.effect_id)
            {
                effect.status = update.status;
                effect.attempt_count += update.attempt_delta;
                effect.last_error = update.last_error.clone();
                effect.updated_at = now;
            }

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: committed_effects,
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
        }

        async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_default())
        }
    }

    #[async_trait]
    impl TrackEffectStore for MemoryRepository {
        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| matches!(effect.status, EffectStatus::Pending))
                .cloned()
                .collect())
        }

        async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| {
                    matches!(effect.status, EffectStatus::Pending)
                        && matches!(effect.effect, TrackEffect::SubmitOrder { .. })
                })
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| {
                    effect.track_id == *track_id
                        && matches!(effect.status, EffectStatus::Pending)
                        && matches!(effect.effect, TrackEffect::SubmitOrder { .. })
                })
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track_batch(
            &self,
            track_id: &TrackId,
            batch_id: &str,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| {
                    effect.track_id == *track_id
                        && effect.batch_id == batch_id
                        && matches!(effect.status, EffectStatus::Pending)
                        && matches!(effect.effect, TrackEffect::SubmitOrder { .. })
                })
                .cloned()
                .collect())
        }

        async fn save_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            self.retirement_requests
                .lock()
                .unwrap()
                .entry(track_id.as_str().to_string())
                .or_default()
                .push(request.clone());
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(self
                .retirement_requests
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            if let Some(requests) = self
                .retirement_requests
                .lock()
                .unwrap()
                .get_mut(track_id.as_str())
            {
                requests.retain(|existing| existing != request);
            }
            Ok(())
        }
    }

    pub(crate) fn track_write_services(
        manager: TrackManager,
        repository: Arc<MemoryRepository>,
    ) -> (TrackServiceSet, broadcast::Sender<ApplicationNotification>) {
        let (notifications, _) = broadcast::channel(16);
        let services = TrackServiceSet::new(
            manager,
            repository.clone(),
            repository,
            notifications.clone(),
            Arc::new(NoopGuard),
        );
        (services, notifications)
    }

    pub(crate) fn seeded_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(TestClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: OutOfBandPolicy::Freeze,
                },
                CapacityBudget {
                    max_notional: 3_000.0,
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                },
                ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 5.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            )
            .unwrap();
        manager
    }

    pub(crate) fn manager_with_pending_submit() -> TrackManager {
        let mut manager = seeded_manager();
        manager
            .record_submit_request(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    client_order_id: "client-1".into(),
                    reduce_only: false,
                },
                Exposure(4.0),
            )
            .unwrap();
        manager
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use poise_engine::executor::RecoveryAnomaly;
    use poise_engine::observation::MarketObservation;
    use poise_engine::ports::ExecutionQuote;
    use poise_engine::snapshot::TrackRuntimeSnapshot;
    use poise_engine::track::TrackId;
    use tokio::sync::broadcast;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::test_support::{MemoryRepository, NoopGuard, seeded_manager};
    use super::{MutationExecutor, RecoveryAnomalyObserver};
    use crate::TrackMutationStore;

    #[derive(Default)]
    struct RecordingRecoveryAnomalyObserver {
        updates: Mutex<Vec<(String, bool)>>,
    }

    impl RecordingRecoveryAnomalyObserver {
        fn recorded(&self) -> Vec<(String, bool)> {
            self.updates.lock().unwrap().clone()
        }
    }

    impl RecoveryAnomalyObserver for RecordingRecoveryAnomalyObserver {
        fn observe_recovery_anomaly_change(&self, track_id: &TrackId, active: bool) {
            self.updates
                .lock()
                .unwrap()
                .push((track_id.as_str().to_string(), active));
        }
    }

    fn test_executor(
        repository: Arc<MemoryRepository>,
        observer: Arc<RecordingRecoveryAnomalyObserver>,
    ) -> MutationExecutor {
        let (notifications, _) = broadcast::channel(16);
        MutationExecutor::new(
            seeded_manager(),
            repository.clone(),
            repository,
            notifications,
            Arc::new(NoopGuard),
            observer,
        )
    }

    fn snapshot_with_recovery_anomaly(active: bool) -> TrackRuntimeSnapshot {
        let manager = seeded_manager();
        let mut snapshot = manager.get_track("btc-core").unwrap().snapshot();
        snapshot.executor_state.diagnostics.recovery_anomaly =
            active.then_some(RecoveryAnomaly::UnknownLiveOrder);
        snapshot
    }

    #[tokio::test]
    async fn commit_track_mutation_notifies_recovery_anomaly_activation_edges_only() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer.clone());

        let previous_snapshot = snapshot_with_recovery_anomaly(false);
        let mut next_snapshot = previous_snapshot.clone();
        next_snapshot.current_exposure = poise_core::types::Exposure(1.0);

        executor
            .commit_track_mutation(
                "btc-core",
                &previous_snapshot,
                &next_snapshot,
                &(),
                None,
                false,
            )
            .await
            .unwrap();

        assert!(observer.recorded().is_empty());

        let next_snapshot = snapshot_with_recovery_anomaly(true);
        executor
            .commit_track_mutation(
                "btc-core",
                &previous_snapshot,
                &next_snapshot,
                &(),
                None,
                false,
            )
            .await
            .unwrap();

        assert_eq!(observer.recorded(), vec![("btc-core".to_string(), true)]);
    }

    #[tokio::test]
    async fn commit_track_mutation_notifies_recovery_anomaly_clear_edges_only() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer.clone());

        let previous_snapshot = snapshot_with_recovery_anomaly(true);
        let next_snapshot = snapshot_with_recovery_anomaly(false);

        executor
            .commit_track_mutation(
                "btc-core",
                &previous_snapshot,
                &next_snapshot,
                &(),
                None,
                false,
            )
            .await
            .unwrap();

        assert_eq!(observer.recorded(), vec![("btc-core".to_string(), false)]);
    }

    #[tokio::test]
    async fn mutation_executor_exposes_market_data_health_deadline() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);

        assert_eq!(
            executor
                .market_data_health_deadline("btc-core")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn observe_market_live_only_tick_does_not_emit_track_changed() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = MutationExecutor::new(
            seeded_manager(),
            repository.clone(),
            repository.clone(),
            notifications.clone(),
            Arc::new(NoopGuard),
            observer,
        );
        let mut receiver = notifications.subscribe();

        {
            let manager = executor.manager();
            let mut manager = manager.write().await;
            let track = manager
                .get_track("btc-core")
                .cloned()
                .expect("seeded track should exist");
            let mut updated = track.snapshot();
            updated.status = poise_engine::runtime::TrackStatus::Active;
            updated.current_exposure = poise_core::types::Exposure(2.0);
            updated.desired_exposure = Some(poise_core::types::Exposure(2.0));
            manager
                .restore_track_state(&updated)
                .expect("failed to seed active exposure state");
        }

        let transition = executor
            .observe_market(
                "btc-core",
                MarketObservation {
                    mark_price: 97.0,
                    execution_quote: Some(ExecutionQuote {
                        best_bid: 97.0,
                        best_ask: 97.0,
                    }),
                },
            )
            .await
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert!(
            repository
                .load_track_state("btc-core")
                .await
                .unwrap()
                .is_none(),
            "live-only tick should not persist a durable snapshot"
        );
    }
}
