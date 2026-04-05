use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use poise_core::events::DomainEvent;
use poise_engine::command::TrackCommand;
use poise_engine::executor::{
    OrderSlot, OrderUpdateAbsorbResult, SubmitRecoveryPlan, SubmitRecoveryResolution,
};
use poise_engine::manager::{ExchangeSyncMode, TrackManager};
use poise_engine::observation::{
    MarketObservation, OrderObservation, PositionObservation, TrackObservation,
};
use poise_engine::ports::{
    EffectStatusUpdate, ExchangeOrder, FollowUpRetirementRequest, OrderReceipt, OrderRequest,
    PersistedTrackEffect, StateRepositoryPort,
};
use poise_engine::runtime::AccountCapacityConstraint;
use poise_engine::track::{Instrument, TrackId};
use poise_engine::transition::{TrackEffect, TrackTransition};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast};

use crate::notifications::ServerNotification;
use crate::runtime::AccountMarginGuardStore;

pub type SharedManager = Arc<RwLock<TrackManager>>;

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
pub struct TrackWriteService {
    manager: SharedManager,
    repository: Arc<dyn StateRepositoryPort>,
    mutation_guards: Arc<TrackMutationGuards>,
    notifications: broadcast::Sender<ServerNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInstrument {
    pub id: String,
    pub instrument: Instrument,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSubmitExecution {
    pub desired_exposure: poise_core::types::Exposure,
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

pub(crate) fn is_loaded_track_invariant_violation(error: &anyhow::Error) -> bool {
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

impl TrackWriteService {
    pub fn new(
        manager: TrackManager,
        repository: Arc<dyn StateRepositoryPort>,
        notifications: broadcast::Sender<ServerNotification>,
        account_margin_guard: Arc<AccountMarginGuardStore>,
    ) -> Self {
        Self {
            manager: Arc::new(RwLock::new(manager)),
            repository,
            mutation_guards: Arc::new(TrackMutationGuards::default()),
            notifications,
            account_margin_guard,
        }
    }

    #[cfg(test)]
    pub fn manager(&self) -> SharedManager {
        Arc::clone(&self.manager)
    }

    pub fn subscribe_notifications(&self) -> broadcast::Receiver<ServerNotification> {
        self.notifications.subscribe()
    }

    pub fn notification_sender(&self) -> broadcast::Sender<ServerNotification> {
        self.notifications.clone()
    }

    pub(crate) fn uses_account_margin_guard(
        &self,
        account_margin_guard: &Arc<AccountMarginGuardStore>,
    ) -> bool {
        Arc::ptr_eq(&self.account_margin_guard, account_margin_guard)
    }

    #[cfg(test)]
    pub(crate) fn account_margin_guard(&self) -> Arc<AccountMarginGuardStore> {
        Arc::clone(&self.account_margin_guard)
    }

    pub(crate) fn emit_internal_notification(&self, notification: ServerNotification) {
        let _ = self.notifications.send(notification);
    }

    pub async fn has_track(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_track(id).is_some()
    }

    pub async fn track_instruments(&self) -> Vec<TrackInstrument> {
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

    pub async fn resolve_track_id(&self, instrument: &Instrument) -> Option<String> {
        let manager = self.manager.read().await;
        manager
            .resolve_track_id(instrument)
            .map(|track_id| track_id.as_str().to_string())
    }

    pub async fn observe_market(&self, id: &str, reference_price: f64) -> Result<TrackTransition> {
        self.mutate_track(id, |manager| {
            manager.observe(
                &TrackId::new(id),
                TrackObservation::Market(MarketObservation { reference_price }),
            )
        })
        .await
        .map_err(anyhow::Error::new)
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
            .repository
            .list_pending_submit_effects_for_track(&TrackId::new(id))
            .await
            .map_err(TrackMutationError::Persistence)?
            .into_iter()
            .filter_map(|effect| match effect.effect {
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                } => Some(poise_engine::executor::PendingSubmitHint {
                    request,
                    desired_exposure,
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

    pub async fn complete_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
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
        self.mutate_track_with_effect_status(
            id,
            effect_status_failed(effect_id, error),
            |manager| {
                manager.record_submit_failure(&TrackId::new(id), client_order_id)?;
                Ok(())
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
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
        self.repository
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
            .repository
            .list_follow_up_retirement_requests(&TrackId::new(id))
            .await?
        {
            if self.retire_stale_follow_up_submit(id, &request).await? {
                self.repository
                    .delete_follow_up_retirement_request(&TrackId::new(id), &request)
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn prepare_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<Option<PreparedSubmitExecution>> {
        Ok(
            match self
                .recover_submit_effect(id, effect_id, request, desired_exposure, live_order)
                .await?
            {
                SubmitRecoveryResolution::Proceed {
                    desired_exposure, ..
                } => Some(PreparedSubmitExecution { desired_exposure }),
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

    async fn lock_track_mutation(&self, id: &str) -> OwnedMutexGuard<()> {
        self.mutation_guards.lock(id).await
    }

    fn sync_account_capacity_constraint(&self, manager: &mut TrackManager, id: &str) -> Result<()> {
        let Some(mut snapshot) = manager.snapshot(id) else {
            return Ok(());
        };
        let constraint = self
            .account_margin_guard
            .constraint_for(&snapshot.instrument);
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
        self.repository
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
                manager.restore_track_state(previous_snapshot)
            };
            if let Err(rollback_error) = rollback_result {
                return Err(TrackMutationError::Persistence(anyhow!(
                    "failed to persist track `{id}`: {error}; rollback failed: {rollback_error}"
                )));
            }
            return Err(TrackMutationError::Persistence(error));
        }

        if has_track_write || has_effect_status_update {
            self.emit_internal_notification(ServerNotification::TrackChanged {
                track_id: TrackId::new(id),
            });
        }

        Ok(())
    }
}

fn effect_status_failed(effect_id: &str, error: &str) -> EffectStatusUpdate {
    EffectStatusUpdate {
        effect_id: effect_id.to_string(),
        status: poise_engine::ports::EffectStatus::Failed,
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
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::command::TrackCommand;
    use poise_engine::executor::{
        ExecutionMode, OrderRole, OrderSlot, OrderUpdateAbsorbResult, SubmitRecoveryResolution,
    };
    use poise_engine::manager::{ExchangeSyncMode, TrackManager};
    use poise_engine::observation::{
        MarketObservation, OrderObservation, PositionObservation, TrackObservation,
    };
    use poise_engine::ports::{
        ClockPort, CommittedTrackWrite, EffectStatus, EffectStatusUpdate, OrderReceipt,
        OrderRequest, OrderStatus, PersistedTrackEffect, StateRepositoryPort,
    };
    use poise_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, SlotState, WorkingOrder,
    };
    use poise_engine::snapshot::TrackRuntimeSnapshot;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use tokio::sync::Notify;
    use tokio::time::timeout;

    use crate::notifications::ServerNotification;
    use crate::runtime::AccountMarginGuardStore;
    use crate::write_service::FollowUpRetirementRequest;

    use super::TrackWriteService;

    #[tokio::test]
    async fn mutate_track_persists_tick_events_and_emits_notification_after_save() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

        let outcome = service
            .mutate_track("btc-core", |manager| {
                manager.observe(
                    &TrackId::new("btc-core"),
                    TrackObservation::Market(MarketObservation {
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
            ServerNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
    }

    #[tokio::test]
    async fn constructor_injected_guard_is_used_when_syncing_capacity_constraint() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        service.account_margin_guard().activate_insufficient_margin(
            &Instrument::new(Venue::Binance, "BTCUSDT"),
            "insufficient_margin",
            Utc.with_ymd_and_hms(2026, 4, 5, 0, 0, 0).unwrap(),
        );

        service.observe_market("btc-core", 95.0).await.unwrap();

        let snapshot = service
            .manager()
            .read()
            .await
            .snapshot("btc-core")
            .expect("track snapshot should exist");
        assert!(snapshot.risk.account_capacity_constraint.increase_blocked);
        assert_eq!(
            snapshot
                .risk
                .account_capacity_constraint
                .blocked_reason
                .as_deref(),
            Some("insufficient_margin")
        );
    }

    #[tokio::test]
    async fn mutate_track_persists_engine_snapshot_without_server_side_snapshot_builder() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        service
            .mutate_track("btc-core", |manager| {
                manager.observe(
                    &TrackId::new("btc-core"),
                    TrackObservation::Market(MarketObservation {
                        reference_price: 95.0,
                    }),
                )
            })
            .await
            .unwrap();

        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        let expected = manager.get_track("btc-core").unwrap().snapshot();

        assert_eq!(repository.snapshot_for("btc-core"), Some(expected));
    }

    #[tokio::test]
    async fn mutate_track_persists_effects_with_snapshot_and_events() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let transition = service.observe_market("btc-core", 95.0).await.unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
        );

        let pending = repository.pending_effects();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].track_id.as_str(), "btc-core");
        assert!(matches!(pending[0].effect, TrackEffect::SubmitOrder { .. }));
        assert_eq!(pending[0].status, EffectStatus::Pending);
        assert_eq!(repository.events_for("btc-core"), transition.events);
        assert!(repository.snapshot_for("btc-core").is_some());
    }

    #[tokio::test]
    async fn mutate_track_rolls_back_and_does_not_broadcast_when_save_fails() {
        let repository = Arc::new(FailOnSaveRepository::default());
        let service = test_service(repository as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

        let error = match service
            .mutate_track("btc-core", |manager| {
                manager.observe(
                    &TrackId::new("btc-core"),
                    TrackObservation::Market(MarketObservation {
                        reference_price: 95.0,
                    }),
                )
            })
            .await
        {
            Ok(_) => panic!("mutation should fail when save fails"),
            Err(error) => error,
        };
        assert!(matches!(error, super::TrackMutationError::Persistence(_)));

        let manager_handle = service.manager();
        let manager = manager_handle.read().await;
        let snapshot = manager.snapshot("btc-core").unwrap();
        assert_eq!(snapshot.desired_exposure, None);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), receiver.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn command_persists_transition_and_emits_track_write_committed() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);
        let mut receiver = service.subscribe_notifications();

        service
            .command("btc-core", TrackCommand::Pause)
            .await
            .unwrap();

        let notification = receiver.recv().await.unwrap();
        assert_eq!(
            notification,
            ServerNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
    }

    #[test]
    fn exchange_sync_mode_explicitly_controls_follow_up_reconcile() {
        assert!(!ExchangeSyncMode::RecoverOnly.allows_follow_up_reconcile());
        assert!(ExchangeSyncMode::RecoverAndReconcile.allows_follow_up_reconcile());
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
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: poise_engine::ports::OrderStatus::New,
                }],
            )
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            ServerNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
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
            "startup sync should read track-scoped pending submit hints instead of the global pending effect list",
        );
        assert_eq!(
            repository.pending_submit_hint_queries(),
            vec!["btc-core".to_string()]
        );
    }

    #[tokio::test]
    async fn mutations_for_same_track_remain_serialized() {
        let repository = Arc::new(MemoryRepository::default());
        repository.block_next_save("btc-core");
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        let first_service = service.clone();
        let first_mutation =
            tokio::spawn(async move { first_service.observe_market("btc-core", 95.0).await });
        repository.wait_for_save_started("btc-core", 1).await;

        let second_service = service.clone();
        let second_mutation = tokio::spawn(async move {
            second_service
                .command("btc-core", TrackCommand::Pause)
                .await
        });

        assert!(
            timeout(
                Duration::from_millis(100),
                repository.wait_for_save_started("btc-core", 2),
            )
            .await
            .is_err(),
            "same-track mutation should wait for the in-flight commit"
        );

        repository.release_save("btc-core");

        first_mutation.await.unwrap().unwrap();
        second_mutation.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn mutations_for_different_tracks_do_not_share_global_lock() {
        let repository = Arc::new(MemoryRepository::default());
        repository.block_next_save("btc-core");
        let service = multi_track_service(
            repository.clone() as Arc<dyn StateRepositoryPort>,
            &[("btc-core", "BTCUSDT"), ("eth-core", "ETHUSDT")],
        );

        let first_service = service.clone();
        let first_mutation =
            tokio::spawn(async move { first_service.observe_market("btc-core", 95.0).await });
        repository.wait_for_save_started("btc-core", 1).await;

        let second_service = service.clone();
        let second_mutation = tokio::spawn(async move {
            second_service
                .command("eth-core", TrackCommand::Pause)
                .await
        });

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
            "different tracks should not share a global write lock"
        );
        assert_eq!(completed_snapshot, vec!["eth-core".to_string()]);
    }

    #[tokio::test]
    async fn recover_submit_effect_uses_same_per_track_guard_as_regular_mutations() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_track_service(
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
        snapshot.desired_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order_from_submit_request(&request, Exposure(6.0)),
            SlotState::SubmitPending,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
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
        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:recovery:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: TrackEffect::SubmitOrder {
                request: request.clone(),
                desired_exposure: Exposure(6.0),
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
        let recover_desired_exposure = Exposure(6.0);
        let recover_task = tokio::spawn(async move {
            recover_service
                .recover_submit_effect(
                    "btc-core",
                    "btc-core:recovery:0",
                    &recover_request,
                    recover_desired_exposure,
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
            "recover_submit_effect should reach persistence before this test checks cross-track isolation"
        );

        let other_track_service = service.clone();
        let other_track_mutation = tokio::spawn(async move {
            other_track_service
                .command("eth-core", TrackCommand::Pause)
                .await
        });

        let other_track_completed = timeout(
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
        other_track_mutation.await.unwrap().unwrap();

        assert!(
            other_track_completed.is_ok(),
            "other tracks should remain writable while recovery is persisting"
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
        snapshot.desired_exposure = Some(Exposure(4.0));
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
            manager.restore_track_state(&snapshot).unwrap();
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
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: poise_engine::ports::OrderStatus::New,
                }],
            )
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![]);

        let snapshot = repository.snapshot_for("btc-core").unwrap();
        let executor_state = snapshot.executor_state;
        assert_eq!(
            executor_state.slots,
            vec![poise_engine::runtime::ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(poise_engine::runtime::WorkingOrder {
                    order_id: Some("live-1".into()),
                    client_order_id: "restore-1".into(),
                    side: Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
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
            [TrackEffect::SubmitOrder { request, .. }] => request.clone(),
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
    async fn complete_effect_failed_returns_invariant_violation_when_track_is_not_loaded() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_track_service(repository as Arc<dyn StateRepositoryPort>, &[]);

        let error = service
            .complete_effect_failed("btc-core", "btc-core:batch:0", "submit order rejected")
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("loaded-track invariant violated")
        );
        assert!(error.to_string().contains("btc-core"));
        assert!(!error.to_string().contains("track `btc-core` not found"));
    }

    #[tokio::test]
    async fn recover_submit_effect_returns_invariant_violation_when_track_is_not_loaded() {
        let repository = Arc::new(MemoryRepository::default());
        let service = multi_track_service(repository as Arc<dyn StateRepositoryPort>, &[]);

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
            Ok(_) => panic!("submit recovery should fail when track is not loaded"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("loaded-track invariant violated")
        );
        assert!(error.to_string().contains("btc-core"));
        assert!(!error.to_string().contains("track `btc-core` not found"));
    }

    fn working_order(
        order_id: Option<&str>,
        client_order_id: &str,
        side: Side,
        price: f64,
        quantity: f64,
        _desired_exposure: Exposure,
        status: OrderStatus,
    ) -> WorkingOrder {
        WorkingOrder {
            order_id: order_id.map(str::to_string),
            client_order_id: client_order_id.to_string(),
            side,
            price,
            quantity,
            status,
            role: match side {
                Side::Buy => OrderRole::IncreaseInventory,
                Side::Sell => OrderRole::DecreaseInventory,
            },
        }
    }

    fn working_order_from_submit_request(
        request: &OrderRequest,
        desired_exposure: Exposure,
    ) -> WorkingOrder {
        working_order(
            None,
            &request.client_order_id,
            request.side,
            request.price,
            request.quantity,
            desired_exposure,
            OrderStatus::Submitting,
        )
    }

    fn set_executor_state(
        snapshot: &mut TrackRuntimeSnapshot,
        order: WorkingOrder,
        state: SlotState,
    ) {
        let desired_exposure = snapshot
            .desired_exposure
            .clone()
            .unwrap_or_else(|| snapshot.current_exposure.clone());
        snapshot.executor_state = ExecutorState {
            active_round: Some(poise_engine::runtime::ExecutionRound {
                desired_exposure: desired_exposure.clone(),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            }),
            diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: snapshot.current_exposure.delta(&desired_exposure),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: None,
                recovery_anomaly: None,
            },
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state,
                working_order: Some(order),
            }],
            recent_terminal_orders: Vec::new(),
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
        let (request, desired_exposure) = match transition.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                },
            ] => (request.clone(), desired_exposure.clone()),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        let effect_id = repository.pending_effects()[0].effect_id.clone();

        let resolution = service
            .recover_submit_effect(
                "btc-core",
                &effect_id,
                &request,
                desired_exposure.clone(),
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
                desired_exposure.clone()
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
            Some(working_order_from_submit_request(
                &request,
                desired_exposure
            ))
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
        snapshot.desired_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order_from_submit_request(&request, Exposure(6.0)),
            SlotState::SubmitPending,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
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
        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:recovery:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: TrackEffect::SubmitOrder {
                request: request.clone(),
                desired_exposure: Exposure(6.0),
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
            TrackEffect::SubmitOrder {
                request,
                desired_exposure,
            } if request.side == Side::Buy
                && (request.price - 95.0).abs() < f64::EPSILON
                && (request.quantity - snapshot.config.base_qty_per_unit() * 4.0).abs() < f64::EPSILON
                && *desired_exposure == Exposure(4.0)
        ));
        let replacement_pending = match &replacement.effect {
            TrackEffect::SubmitOrder {
                request,
                desired_exposure,
            } => Some(working_order_from_submit_request(
                request,
                desired_exposure.clone(),
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
            vec![TrackEffect::NoOp],
            "replacement submit should keep suppressing duplicate submit plans before worker pickup"
        );
    }

    #[tokio::test]
    async fn record_cancel_order_success_restores_ready_replacement_submit_before_command_reconcile()
     {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            client_order_id: "btc-core-replacement".into(),
            side: Side::Buy,
            price: 95.0,
            quantity: snapshot.config.base_qty_per_unit() * 4.0,
            reduce_only: false,
        };
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.current_exposure = Exposure(0.0);
        snapshot.desired_exposure = Some(Exposure(4.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: old_order_id.into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: request.clone(),
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .record_cancel_order_success(
                "btc-core",
                "btc-core:replacement:0",
                "replacement",
                0,
                old_order_id,
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:replacement:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Succeeded)
        );
        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .map(|snapshot| snapshot.executor_state.slots),
            Some(vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: Some(working_order_from_submit_request(&request, Exposure(4.0),)),
            }])
        );

        let transition = service
            .command("btc-core", TrackCommand::Reconcile)
            .await
            .unwrap();
        assert_eq!(
            transition.effects,
            vec![TrackEffect::NoOp],
            "replacement submit restored by cancel success should suppress manual reconcile"
        );
        assert_eq!(repository.all_effects().len(), 2);
    }

    #[tokio::test]
    async fn record_cancel_order_success_restores_same_batch_replacement_submit() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let expected_request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            client_order_id: "btc-core-replacement".into(),
            side: Side::Buy,
            price: 95.0,
            quantity: snapshot.config.base_qty_per_unit() * 4.0,
            reduce_only: false,
        };
        let unrelated_request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            client_order_id: "btc-core-unrelated".into(),
            side: Side::Buy,
            price: 96.0,
            quantity: snapshot.config.base_qty_per_unit() * 2.0,
            reduce_only: false,
        };
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.current_exposure = Exposure(0.0);
        snapshot.desired_exposure = Some(Exposure(4.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:other:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "other".into(),
            sequence: 0,
            effect: TrackEffect::SubmitOrder {
                request: unrelated_request,
                desired_exposure: Exposure(2.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: old_order_id.into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: expected_request.clone(),
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .record_cancel_order_success(
                "btc-core",
                "btc-core:replacement:0",
                "replacement",
                0,
                old_order_id,
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .snapshot_for("btc-core")
                .and_then(|snapshot| snapshot.executor_state.slots.into_iter().next())
                .and_then(|slot| slot.working_order)
                .map(|order| order.client_order_id),
            Some(expected_request.client_order_id)
        );
    }

    #[tokio::test]
    async fn record_cancel_order_success_rolls_back_when_atomic_save_fails() {
        let repository = Arc::new(FailOnSaveRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            client_order_id: "btc-core-replacement".into(),
            side: Side::Buy,
            price: 95.0,
            quantity: snapshot.config.base_qty_per_unit() * 4.0,
            reduce_only: false,
        };
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.current_exposure = Exposure(0.0);
        snapshot.desired_exposure = Some(Exposure(4.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request,
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        let error = service
            .record_cancel_order_success(
                "btc-core",
                "btc-core:replacement:0",
                "replacement",
                0,
                old_order_id,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("injected save failure"));
        assert_eq!(
            service
                .manager()
                .read()
                .await
                .snapshot("btc-core")
                .unwrap()
                .executor_state
                .slots,
            snapshot.executor_state.slots
        );
    }

    #[tokio::test]
    async fn pending_follow_up_retirement_retries_after_terminal_order_update() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.observed.reference_price = None;
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "btc-core-replacement".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .request_follow_up_retirement(
                "btc-core",
                FollowUpRetirementRequest {
                    batch_id: "replacement".into(),
                    blocked_sequence: 0,
                    closed_order_id: old_order_id.into(),
                },
            )
            .await
            .unwrap();

        let (_, absorb_result) = service
            .observe_order_with_absorb_result(
                "btc-core",
                OrderObservation {
                    order_id: old_order_id.into(),
                    client_order_id: "old-client-order".into(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.6,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                },
            )
            .await
            .unwrap();

        assert_eq!(absorb_result, OrderUpdateAbsorbResult::Applied);
        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:replacement:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
    }

    #[tokio::test]
    async fn persisted_follow_up_retirement_survives_service_restart_and_retries_on_sync() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.observed.reference_price = None;
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot.clone());
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "btc-core-replacement".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .request_follow_up_retirement(
                "btc-core",
                FollowUpRetirementRequest {
                    batch_id: "replacement".into(),
                    blocked_sequence: 0,
                    closed_order_id: old_order_id.into(),
                },
            )
            .await
            .unwrap();

        let restarted = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        {
            let manager_handle = restarted.manager();
            let mut manager = manager_handle.write().await;
            let mut closed_snapshot = snapshot;
            closed_snapshot.executor_state =
                ExecutorState::empty(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap());
            manager.restore_track_state(&closed_snapshot).unwrap();
            repository.seed_snapshot("btc-core", closed_snapshot);
        }

        restarted
            .sync_exchange_state_without_reconcile(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:replacement:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
    }

    #[tokio::test]
    async fn requested_follow_up_retirement_retires_immediately_when_lifecycle_already_closed() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        snapshot.observed.reference_price = None;
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "btc-core-replacement".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        let (_, absorb_result) = service
            .observe_order_with_absorb_result(
                "btc-core",
                OrderObservation {
                    order_id: old_order_id.into(),
                    client_order_id: "old-client-order".into(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: stale_quantity,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                },
            )
            .await
            .unwrap();
        assert_eq!(absorb_result, OrderUpdateAbsorbResult::Applied);

        service
            .request_follow_up_retirement(
                "btc-core",
                FollowUpRetirementRequest {
                    batch_id: "replacement".into(),
                    blocked_sequence: 0,
                    closed_order_id: old_order_id.into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:replacement:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
    }

    #[tokio::test]
    async fn request_follow_up_retirement_surfaces_immediate_retry_errors() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);

        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "replacement-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:replacement:2".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "replacement".into(),
            sequence: 2,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "replacement-2".into(),
                    side: Side::Buy,
                    price: 96.0,
                    quantity: 0.5,
                    reduce_only: false,
                },
                desired_exposure: Exposure(3.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        let request = FollowUpRetirementRequest {
            batch_id: "replacement".into(),
            blocked_sequence: 0,
            closed_order_id: "old-order-1".into(),
        };
        let error = service
            .request_follow_up_retirement("btc-core", request)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple replacement submit effects found after sequence 0")
        );
    }

    #[tokio::test]
    async fn retry_pending_follow_up_retirements_surfaces_selection_errors() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "broken-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:2".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 2,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "broken-2".into(),
                    side: Side::Buy,
                    price: 96.0,
                    quantity: 0.5,
                    reduce_only: false,
                },
                desired_exposure: Exposure(3.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        repository
            .save_follow_up_retirement_request(
                &TrackId::new("btc-core"),
                &FollowUpRetirementRequest {
                    batch_id: "broken".into(),
                    blocked_sequence: 0,
                    closed_order_id: "order-1".into(),
                },
            )
            .await
            .unwrap();

        let error = service
            .retry_pending_follow_up_retirements("btc-core")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("multiple replacement submit effects found after sequence 0")
        );
    }

    #[tokio::test]
    async fn record_cancel_order_success_keeps_main_writeback_succeeded_when_follow_up_retry_errors()
     {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone() as Arc<dyn StateRepositoryPort>);
        let manager_handle = service.manager();
        let mut snapshot = {
            let manager = manager_handle.read().await;
            manager.snapshot("btc-core").unwrap()
        };
        let old_order_id = "old-order-1";
        let stale_quantity = snapshot.config.base_qty_per_unit() * 6.0;
        set_executor_state(
            &mut snapshot,
            working_order(
                Some(old_order_id),
                "old-client-order",
                Side::Buy,
                94.0,
                stale_quantity,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        {
            let mut manager = manager_handle.write().await;
            manager.restore_track_state(&snapshot).unwrap();
        }
        repository.seed_snapshot("btc-core", snapshot);
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:batch:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: old_order_id.into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 0,
            effect: TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: old_order_id.into(),
            },
            status: EffectStatus::Failed,
            attempt_count: 1,
            last_error: Some(
                "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}"
                    .into(),
            ),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "broken-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });
        repository.seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:2".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 2,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    client_order_id: "broken-2".into(),
                    side: Side::Buy,
                    price: 96.0,
                    quantity: 0.5,
                    reduce_only: false,
                },
                desired_exposure: Exposure(3.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        });

        service
            .record_cancel_order_success("btc-core", "btc-core:batch:0", "batch", 0, old_order_id)
            .await
            .unwrap();

        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:batch:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Succeeded)
        );
        assert_eq!(
            repository
                .all_effects()
                .iter()
                .find(|effect| effect.effect_id == "btc-core:broken:1")
                .map(|effect| effect.status),
            Some(EffectStatus::Pending)
        );
    }

    #[tokio::test]
    async fn resolves_track_id_from_instrument() {
        let service =
            test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>);

        let track_id = service
            .resolve_track_id(&Instrument::new(Venue::Binance, "BTCUSDT"))
            .await;

        assert_eq!(track_id, Some("btc-core".to_string()));
    }

    fn test_service(repository: Arc<dyn StateRepositoryPort>) -> TrackWriteService {
        multi_track_service(repository, &[("btc-core", "BTCUSDT")])
    }

    fn multi_track_service(
        repository: Arc<dyn StateRepositoryPort>,
        tracks: &[(&str, &str)],
    ) -> TrackWriteService {
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let mut manager = TrackManager::new(Arc::new(FixedClock));
        for (id, symbol) in tracks {
            manager
                .add_track(
                    TrackId::new(*id),
                    Instrument::new(Venue::Binance, *symbol),
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

        TrackWriteService::new(
            manager,
            repository,
            notifications,
            Arc::new(AccountMarginGuardStore::default()),
        )
    }

    #[derive(Default)]
    struct MemoryRepository {
        snapshots: Mutex<HashMap<String, TrackRuntimeSnapshot>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedTrackEffect>>,
        follow_up_retirements: Mutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
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

        fn snapshot_for(&self, id: &str) -> Option<TrackRuntimeSnapshot> {
            self.snapshots.lock().unwrap().get(id).cloned()
        }

        fn pending_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect()
        }

        fn all_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().unwrap().clone()
        }

        fn global_pending_effect_queries(&self) -> usize {
            self.global_pending_effect_queries.load(Ordering::SeqCst)
        }

        fn pending_submit_hint_queries(&self) -> Vec<String> {
            self.pending_submit_hint_queries.lock().unwrap().clone()
        }

        fn seed_snapshot(&self, id: &str, snapshot: TrackRuntimeSnapshot) {
            self.snapshots
                .lock()
                .unwrap()
                .insert(id.to_string(), snapshot);
        }

        fn seed_effect(&self, effect: PersistedTrackEffect) {
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
            state: &TrackRuntimeSnapshot,
            events: &[DomainEvent],
            effects: &[TrackEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
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
                if matches!(effect, TrackEffect::NoOp) {
                    continue;
                }

                let persisted = PersistedTrackEffect {
                    effect_id: format!("{id}:{batch_id}:{sequence}"),
                    track_id: TrackId::new(id),
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

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
            Ok(self.snapshots.lock().unwrap().get(id).cloned())
        }

        async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>> {
            Ok(self.events_for(id))
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            self.global_pending_effect_queries
                .fetch_add(1, Ordering::SeqCst);
            Ok(self.pending_effects())
        }

        async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            self.pending_submit_hint_queries
                .lock()
                .unwrap()
                .push(track_id.as_str().to_string());
            Ok(self
                .pending_effects()
                .into_iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
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
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn save_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().unwrap();
            let entry = stored.entry(track_id.clone()).or_default();
            if !entry.contains(request) {
                entry.push(request.clone());
            }
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(self
                .follow_up_retirements
                .lock()
                .unwrap()
                .get(track_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().unwrap();
            if let Some(existing) = stored.get_mut(track_id) {
                existing.retain(|candidate| candidate != request);
                if existing.is_empty() {
                    stored.remove(track_id);
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailOnSaveRepository {
        effects: Mutex<Vec<PersistedTrackEffect>>,
        follow_up_retirements: Mutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
    }

    impl FailOnSaveRepository {
        fn seed_effect(&self, effect: PersistedTrackEffect) {
            self.effects.lock().unwrap().push(effect);
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSaveRepository {
        async fn save_transition_with_effect_status(
            &self,
            _id: &str,
            _state: &TrackRuntimeSnapshot,
            _events: &[DomainEvent],
            _effects: &[TrackEffect],
            _effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            Err(anyhow!("injected save failure"))
        }

        async fn load_track_state(&self, _id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
            Ok(None)
        }

        async fn list_track_events(&self, _id: &str) -> Result<Vec<DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
        }

        async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(Vec::new())
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
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn save_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().unwrap();
            let entry = stored.entry(track_id.clone()).or_default();
            if !entry.contains(request) {
                entry.push(request.clone());
            }
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(self
                .follow_up_retirements
                .lock()
                .unwrap()
                .get(track_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            track_id: &TrackId,
            request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            let mut stored = self.follow_up_retirements.lock().unwrap();
            if let Some(existing) = stored.get_mut(track_id) {
                existing.retain(|candidate| candidate != request);
                if existing.is_empty() {
                    stored.remove(track_id);
                }
            }
            Ok(())
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
    fn _test_snapshot() -> TrackRuntimeSnapshot {
        test_service(Arc::new(MemoryRepository::default()) as Arc<dyn StateRepositoryPort>)
            .manager
            .blocking_read()
            .get_track("btc-core")
            .unwrap()
            .snapshot()
    }
}
