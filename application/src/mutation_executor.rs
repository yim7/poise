use std::collections::HashMap;
use std::sync::Arc;

use crate::session_effect_queue::{EnqueuedEffectJournalEntry, FollowUpQueueAction, WakeSignal};
use crate::{
    ApplicationNotification, CancelReceiptResolution, EffectJournalEntry, EffectStatus,
    EffectStatusUpdate, SessionEffectQueue, TrackControlCommand, TrackControlState,
    TrackEffectJournal, TrackMutationStore,
};
use anyhow::{Result, anyhow};
use poise_core::events::DomainEvent;
use poise_core::track::{Instrument, TrackId};
use poise_engine::command::TrackCommand;
use poise_engine::execution_plan::TrackEffect;
use poise_engine::executor::{
    OrderUpdateAbsorbResult, SubmitRecoveryPlan, SubmitRecoveryResolution, SubmitRecoveryToken,
};
use poise_engine::ledger::TrackLedgerEvent;
use poise_engine::manager::MarketMutationOutcome;
use poise_engine::manager::{ExchangeSyncMode, TrackManager};
use poise_engine::observation::{
    CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
    TrackObservation,
};
use poise_engine::ports::{ExchangeOrder, OrderReceipt, OrderRequest};
use poise_engine::runtime::{
    FreshSessionExternalInputs, QuoteHealthView, StrategyTargetView, TerminationCause,
    TrackLiveView, TrackRuntimeView, TrackState,
};
use poise_engine::transition::TrackTransition;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast};

use crate::submit_effect_service::{SubmitAttemptResult, SubmitExecutionRecovery};

pub struct TrackServiceSet {
    pub command: crate::TrackCommandService,
    pub observation: crate::TrackObservationService,
    pub effect: crate::TrackEffectService,
    pub submit_effect: crate::submit_effect_service::SubmitEffectService,
    pub runtime_lifecycle: crate::TrackRuntimeLifecycleService,
    pub session_effect_queue: SessionEffectQueue,
}

pub trait AccountCapacityGuard: Send + Sync {
    fn available_notional_for(&self, instrument: &Instrument) -> Option<f64>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FollowUpQueueHandling {
    requires_reconcile: bool,
    exchange_state_wake_consumed: bool,
}

fn effect_journal_entries_from_enqueued(
    entries: Vec<EnqueuedEffectJournalEntry>,
) -> Vec<EffectJournalEntry> {
    entries
        .into_iter()
        .map(|entry| EffectJournalEntry {
            effect_id: entry.effect_id,
            track_id: entry.track_id,
            batch_id: entry.batch_id,
            sequence: entry.sequence,
            effect: entry.effect,
            created_at: entry.created_at,
        })
        .collect()
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
    effect_journal: Arc<dyn TrackEffectJournal>,
    session_effect_queue: SessionEffectQueue,
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
        &[]
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

fn persisted_control_state_after_command(
    current_runtime_state: &TrackState,
    command: &TrackCommand,
) -> Option<TrackControlState> {
    let current_control_state =
        TrackControlState::from_runtime_state_for_write(current_runtime_state);
    let control_command = match command {
        TrackCommand::Pause => TrackControlCommand::Pause,
        TrackCommand::Resume => TrackControlCommand::Resume,
        TrackCommand::Terminate => TrackControlCommand::Terminate {
            cause: TerminationCause::ManualCommand,
        },
        TrackCommand::Flatten => TrackControlCommand::ManualFlatten,
        TrackCommand::Reconcile => return None,
    };

    let next_control_state =
        TrackControlState::from_command(current_control_state.clone(), control_command);
    if next_control_state == current_control_state {
        None
    } else {
        Some(next_control_state)
    }
}

fn persisted_control_state_after_transition(
    previous_runtime_state: &TrackState,
    next_runtime_state: &TrackState,
    explicit_override: Option<&TrackControlState>,
) -> Option<TrackControlState> {
    if let Some(control_state) = explicit_override {
        return Some(control_state.clone());
    }

    match next_runtime_state {
        TrackState::Terminated { cause }
            if !matches!(
                previous_runtime_state,
                TrackState::Terminated {
                    cause: previous_cause,
                } if previous_cause == cause
            ) =>
        {
            Some(TrackControlState::Terminated {
                cause: cause.clone(),
            })
        }
        _ => None,
    }
}

impl MutationExecutor {
    pub(crate) fn new(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_journal: Arc<dyn TrackEffectJournal>,
        session_effect_queue: SessionEffectQueue,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        Self {
            manager: Arc::new(RwLock::new(manager)),
            mutation_store,
            effect_journal,
            session_effect_queue,
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

    pub(crate) async fn prepare_fresh_session_for_activation(&self, id: &str) -> Result<()> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let track_id = TrackId::new(id);
        self.session_effect_queue.clear_track(&track_id);

        let mut manager = self.manager.write().await;
        manager
            .reset_executor_for_activation(&track_id)
            .map_err(TrackMutationError::Mutation)?;
        Ok(())
    }

    pub(crate) async fn fresh_start_track_runtime(
        &self,
        track_id: &TrackId,
        control_state: TrackControlState,
        ledger_state: poise_engine::ledger::TrackLedgerState,
        external_inputs: FreshSessionExternalInputs,
    ) -> Result<bool> {
        let _mutation_guard = self.lock_track_mutation(track_id.as_str()).await;
        let mut manager = self.manager.write().await;
        if manager.get_track(track_id.as_str()).is_none() {
            return Ok(false);
        }
        manager.fresh_start_track(
            track_id,
            control_state.to_startup_runtime_state(),
            ledger_state,
            external_inputs,
        )?;
        Ok(true)
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
        query_store: Arc<dyn crate::TrackQueryStore>,
        effect_store: Arc<dyn TrackEffectJournal>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
    ) -> Self {
        Self::new_with_recovery_anomaly_observer(
            manager,
            mutation_store,
            query_store,
            effect_store,
            notifications,
            account_margin_guard,
            Arc::new(NoopRecoveryAnomalyObserver),
        )
    }

    pub fn new_with_recovery_anomaly_observer(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        query_store: Arc<dyn crate::TrackQueryStore>,
        effect_store: Arc<dyn TrackEffectJournal>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        let session_effect_queue = SessionEffectQueue::default();
        let executor = Arc::new(MutationExecutor::new(
            manager,
            mutation_store,
            effect_store,
            session_effect_queue.clone(),
            notifications,
            account_margin_guard,
            recovery_anomaly_observer,
        ));
        let observation = Arc::new(crate::TrackObservationService::from_executor(
            executor.clone(),
        ));
        Self {
            command: crate::TrackCommandService::from_executor(executor.clone()),
            observation: observation.as_ref().clone(),
            effect: crate::TrackEffectService::from_executor(executor.clone()),
            submit_effect: crate::submit_effect_service::SubmitEffectService::from_executor(
                executor.clone(),
            ),
            runtime_lifecycle: crate::TrackRuntimeLifecycleService::from_executor(
                executor,
                query_store,
                observation,
            ),
            session_effect_queue,
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
        let previous_frame = {
            let manager = self.manager.read().await;
            manager
                .mutation_frame(id)
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
                let next_frame = {
                    let manager = self.manager.read().await;
                    manager
                        .mutation_frame(id)
                        .ok_or_else(|| anyhow!("track `{id}` not found"))?
                };
                self.session_effect_queue
                    .wake_track_for(&TrackId::new(id), WakeSignal::FreshMarket);

                Ok(TrackTransition {
                    frame: next_frame,
                    events: Vec::new(),
                    effects: Vec::new(),
                })
            }
            MarketMutationOutcome::Durable(transition) => {
                let transition = *transition;
                self.commit_track_mutation(
                    id,
                    &previous_frame,
                    &transition.frame,
                    &transition,
                    None,
                    false,
                    None,
                )
                .await
                .map_err(anyhow::Error::new)?;
                self.session_effect_queue
                    .wake_track_for(&TrackId::new(id), WakeSignal::FreshMarket);
                Ok(transition)
            }
        }
    }

    pub async fn command(&self, id: &str, command: TrackCommand) -> Result<TrackTransition> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let (previous_frame, transition, next_frame, control_state_override) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            self.sync_account_capacity_gate_state(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let control_state_override =
                persisted_control_state_after_command(previous_frame.runtime_state(), &command);
            let transition = manager
                .command(&TrackId::new(id), command)
                .map_err(|error| {
                    manager
                        .rollback_track_state(&previous_frame)
                        .expect("failed to restore previous frame after mutation error");
                    TrackMutationError::Mutation(error)
                })?;
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            (
                previous_frame,
                transition,
                next_frame,
                control_state_override,
            )
        };

        self.commit_track_mutation(
            id,
            &previous_frame,
            &next_frame,
            &transition,
            None,
            false,
            control_state_override.as_ref(),
        )
        .await
        .map_err(anyhow::Error::new)?;
        Ok(transition)
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

    pub(crate) async fn track_live_view(&self, id: &str) -> Result<TrackLiveView> {
        let manager = self.manager.read().await;
        manager.track_live_view(&TrackId::new(id))
    }

    pub(crate) async fn track_runtime_view(&self, id: &str) -> Result<Option<TrackRuntimeView>> {
        let manager = self.manager.read().await;
        Ok(manager.get_track(id).map(|track| track.runtime_view()))
    }

    pub(crate) async fn quote_health_view(&self, id: &str) -> Result<QuoteHealthView> {
        let manager = self.manager.read().await;
        manager.quote_health_view(&TrackId::new(id))
    }

    pub(crate) async fn strategy_target_view(&self, id: &str) -> Result<StrategyTargetView> {
        let manager = self.manager.read().await;
        manager.strategy_target_view(&TrackId::new(id))
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
        open_orders: CompleteOpenOrderSnapshot,
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
        open_orders: CompleteOpenOrderSnapshot,
    ) -> Result<TrackTransition> {
        self.sync_exchange_state_inner(id, position, open_orders, ExchangeSyncMode::RecoverOnly)
            .await
    }

    async fn sync_exchange_state_inner(
        &self,
        id: &str,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
        mode: ExchangeSyncMode,
    ) -> Result<TrackTransition> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let track_id = TrackId::new(id);
        let follow_up_plan = self
            .session_effect_queue
            .plan_cancel_follow_ups_from_open_order_snapshot(&track_id, &open_orders);
        let effective_mode = if follow_up_plan.requires_reconcile() {
            ExchangeSyncMode::RecoverAndReconcile
        } else {
            mode
        };
        let pending_submit_hints = self
            .session_effect_queue
            .active_submit_hints_for_track(&track_id);
        let (previous_frame, transition, next_frame) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            self.sync_account_capacity_gate_state(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let transition = if effective_mode.allows_follow_up_reconcile() {
                manager
                    .sync_exchange_state(
                        &TrackId::new(id),
                        position,
                        open_orders.clone(),
                        pending_submit_hints,
                    )
                    .map_err(|error| {
                        manager
                            .rollback_track_state(&previous_frame)
                            .expect("failed to restore previous frame after sync_exchange_state mutation error");
                        TrackMutationError::Mutation(error)
                    })?
            } else {
                manager
                    .sync_exchange_state_without_reconcile(
                        &TrackId::new(id),
                        position,
                        open_orders.clone(),
                        pending_submit_hints,
                    )
                    .map_err(|error| {
                        manager
                            .rollback_track_state(&previous_frame)
                            .expect("failed to restore previous frame after sync_exchange_state_without_reconcile mutation error");
                        TrackMutationError::Mutation(error)
                    })?
            };
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            (previous_frame, transition, next_frame)
        };

        self.commit_track_mutation(
            id,
            &previous_frame,
            &next_frame,
            &transition,
            None,
            false,
            None,
        )
        .await
        .map_err(anyhow::Error::new)?;

        let follow_up_actions = self
            .session_effect_queue
            .commit_cancel_follow_up_resolution(follow_up_plan);
        let follow_up_handling = self
            .handle_follow_up_queue_actions(id, &follow_up_actions)
            .await?;

        let resolved_submit_effect_ids = self
            .session_effect_queue
            .resolve_submitted_awaiting_exchange_state_for_track(&track_id);
        self.record_effects_succeeded(id, &resolved_submit_effect_ids)
            .await?;

        if !follow_up_handling.exchange_state_wake_consumed {
            self.session_effect_queue
                .wake_track_for(&track_id, WakeSignal::ExchangeState);
        }

        Ok(transition)
    }

    async fn handle_follow_up_queue_actions(
        &self,
        id: &str,
        actions: &[FollowUpQueueAction],
    ) -> std::result::Result<FollowUpQueueHandling, TrackMutationError> {
        let mut requires_reconcile = false;
        let mut exchange_state_wake_consumed = false;
        let mut journal_outcomes = Vec::new();

        for action in actions {
            match action {
                FollowUpQueueAction::Closed {
                    cancel_effect_id,
                    superseded_downstream_effect_ids,
                    requires_reconcile: action_requires_reconcile,
                } => {
                    requires_reconcile |= *action_requires_reconcile;
                    journal_outcomes.push(EffectStatusUpdate::succeeded(cancel_effect_id.clone()));
                    journal_outcomes.extend(
                        superseded_downstream_effect_ids
                            .iter()
                            .cloned()
                            .map(EffectStatusUpdate::superseded),
                    );
                }
                FollowUpQueueAction::StillOpen { .. } => {
                    exchange_state_wake_consumed = true;
                }
                FollowUpQueueAction::Blocked { reason } => {
                    return Err(TrackMutationError::Mutation(anyhow!(
                        "failed to resolve cancel follow-ups for track `{id}`: {reason}"
                    )));
                }
            }
        }

        if !journal_outcomes.is_empty()
            && let Err(error) = self
                .effect_journal
                .record_effect_outcomes(&journal_outcomes)
                .await
        {
            tracing::warn!(
                track_id = id,
                "failed to record cancel follow-up journal outcomes: {error}"
            );
        }

        Ok(FollowUpQueueHandling {
            requires_reconcile,
            exchange_state_wake_consumed,
        })
    }

    pub(crate) async fn record_effects_superseded(
        &self,
        id: &str,
        effect_ids: &[String],
    ) -> Result<()> {
        self.record_effect_outcomes_best_effort(
            id,
            effect_ids
                .iter()
                .cloned()
                .map(EffectStatusUpdate::superseded)
                .collect(),
            "superseded",
        )
        .await
    }

    async fn record_effects_succeeded(&self, id: &str, effect_ids: &[String]) -> Result<()> {
        self.record_effect_outcomes_best_effort(
            id,
            effect_ids
                .iter()
                .cloned()
                .map(EffectStatusUpdate::succeeded)
                .collect(),
            "succeeded",
        )
        .await
    }

    async fn record_effect_outcomes_best_effort(
        &self,
        id: &str,
        outcomes: Vec<EffectStatusUpdate>,
        status_label: &str,
    ) -> Result<()> {
        if outcomes.is_empty() {
            return Ok(());
        }

        if let Err(error) = self.effect_journal.record_effect_outcomes(&outcomes).await {
            tracing::warn!(
                track_id = id,
                "failed to record {status_label} effect journal outcomes: {error}"
            );
        }
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: TrackId::new(id),
        });
        Ok(())
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
        order_id: &str,
        receipt: &OrderReceipt,
    ) -> Result<CancelReceiptResolution> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let cancel_effect_status_update = EffectStatusUpdate::succeeded(effect_id.to_string());
        let (previous_frame, next_frame, cancel_progressed) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            manager
                .record_cancel_order_success(&TrackId::new(id), order_id, receipt)
                .map_err(TrackMutationError::Mutation)?;
            let cancel_progressed =
                cancel_receipt_absorbed_exposure(&manager, id, order_id, receipt)
                    .map_err(TrackMutationError::Mutation)?;
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_frame, next_frame, cancel_progressed)
        };

        self.commit_track_mutation_with_effect_status_updates(
            id,
            &previous_frame,
            &next_frame,
            &(),
            std::slice::from_ref(&cancel_effect_status_update),
            false,
            None,
        )
        .await
        .map_err(anyhow::Error::new)?;

        Ok(classify_cancel_receipt_resolution(
            order_id,
            cancel_progressed,
            receipt,
        ))
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

    pub(crate) async fn recover_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        recovery_token: &SubmitRecoveryToken,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitExecutionRecovery> {
        Ok(
            match self
                .recover_submit_effect(id, effect_id, recovery_token, live_order)
                .await?
            {
                SubmitRecoveryResolution::Proceed {
                    request,
                    desired_exposure,
                } => SubmitExecutionRecovery::Dispatch {
                    request,
                    desired_exposure,
                },
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
        recovery_token: &SubmitRecoveryToken,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<SubmitRecoveryResolution> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let (previous_frame, plan, next_frame) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            self.sync_account_capacity_gate_state(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let plan = manager
                .recover_submit_effect(
                    &TrackId::new(id),
                    recovery_token,
                    live_order,
                )
                .map_err(|error| {
                    manager
                        .rollback_track_state(&previous_frame)
                        .expect("failed to restore previous frame after recover_submit_effect mutation error");
                    TrackMutationError::Mutation(error)
                })?;
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_frame, plan, next_frame)
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
            &previous_frame,
            &next_frame,
            &plan,
            effect_status_update.as_ref(),
            true,
            None,
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
        let (previous_frame, result, next_frame) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            self.sync_account_capacity_gate_state(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let result = mutate(&mut manager).map_err(|error| {
                manager
                    .rollback_track_state(&previous_frame)
                    .expect("failed to restore previous frame after mutation error");
                TrackMutationError::Mutation(error)
            })?;
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            (previous_frame, result, next_frame)
        };

        self.commit_track_mutation(
            id,
            &previous_frame,
            &next_frame,
            &result,
            Some(&effect_status_update),
            false,
            None,
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
        let (previous_frame, result, next_frame) = {
            let mut manager = self.manager.write().await;
            let previous_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            self.sync_account_capacity_gate_state(&mut manager, id)
                .map_err(TrackMutationError::Mutation)?;
            let result = mutate(&mut manager).map_err(|error| {
                manager
                    .rollback_track_state(&previous_frame)
                    .expect("failed to restore previous frame after mutation error");
                TrackMutationError::Mutation(error)
            })?;
            let next_frame = manager
                .mutation_frame(id)
                .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
            (previous_frame, result, next_frame)
        };

        self.commit_track_mutation(
            id,
            &previous_frame,
            &next_frame,
            &result,
            None,
            skip_when_noop,
            None,
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
        let previous_frame = manager
            .mutation_frame(id)
            .ok_or_else(|| TrackMutationError::Mutation(anyhow!("track `{id}` not found")))?;
        self.sync_account_capacity_gate_state(&mut manager, id)
            .map_err(TrackMutationError::Mutation)?;
        mutate(&mut manager).map_err(|error| {
            manager
                .rollback_track_state(&previous_frame)
                .expect("failed to restore previous frame after mutation error");
            TrackMutationError::Mutation(error)
        })
    }

    async fn lock_track_mutation(&self, id: &str) -> OwnedMutexGuard<()> {
        self.mutation_guards.lock(id).await
    }

    fn sync_account_capacity_gate_state(&self, manager: &mut TrackManager, id: &str) -> Result<()> {
        let Some(mut snapshot) = manager.mutation_frame(id) else {
            return Ok(());
        };
        let Some(track) = manager.get_track(id) else {
            return Ok(());
        };
        let available_notional = self
            .account_margin_guard
            .available_notional_for(track.instrument());
        if snapshot.account_capacity_available_notional() == available_notional {
            return Ok(());
        }
        snapshot.set_account_capacity_available_notional(available_notional);
        manager.rollback_track_state(&snapshot)
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_track_mutation<R>(
        &self,
        id: &str,
        previous_frame: &poise_engine::mutation_frame::TrackMutationFrame,
        next_frame: &poise_engine::mutation_frame::TrackMutationFrame,
        result: &R,
        effect_status_update: Option<&EffectStatusUpdate>,
        skip_when_noop: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<(), TrackMutationError>
    where
        R: TransitionResult,
    {
        let effect_status_updates = match effect_status_update {
            Some(update) => std::slice::from_ref(update),
            None => &[],
        };
        self.commit_track_mutation_with_effect_status_updates(
            id,
            previous_frame,
            next_frame,
            result,
            effect_status_updates,
            skip_when_noop,
            control_state_override,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_track_mutation_with_effect_status_updates<R>(
        &self,
        id: &str,
        previous_frame: &poise_engine::mutation_frame::TrackMutationFrame,
        next_frame: &poise_engine::mutation_frame::TrackMutationFrame,
        result: &R,
        effect_status_updates: &[EffectStatusUpdate],
        skip_when_noop: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<(), TrackMutationError>
    where
        R: TransitionResult,
    {
        let control_state_update = persisted_control_state_after_transition(
            previous_frame.runtime_state(),
            next_frame.runtime_state(),
            control_state_override,
        );
        let has_session_effects = result
            .effects()
            .iter()
            .any(|effect| !matches!(effect, TrackEffect::NoOp));
        let has_track_write = control_state_update.is_some()
            || next_frame.ledger_changed_since(previous_frame)
            || !result.domain_events().is_empty();
        let has_effect_status_update = !effect_status_updates.is_empty();
        let has_work = has_track_write || has_effect_status_update || has_session_effects;
        if skip_when_noop && !has_work {
            return Ok(());
        }

        if has_track_write {
            let persistence_result = self
                .mutation_store
                .commit_track_transition(
                    id,
                    control_state_update.as_ref(),
                    next_frame.ledger_state(),
                    result.domain_events(),
                )
                .await;

            if let Err(error) = persistence_result {
                let rollback_result = {
                    let mut manager = self.manager.write().await;
                    manager.rollback_track_state(previous_frame)
                };
                if let Err(rollback_error) = rollback_result {
                    return Err(TrackMutationError::Persistence(anyhow!(
                        "failed to persist track `{id}`: {error}; rollback failed: {rollback_error}"
                    )));
                }
                return Err(TrackMutationError::Persistence(error));
            }
        }

        let effect_created_at = chrono::Utc::now();
        let enqueued_effects = self.session_effect_queue.enqueue_transition_effects(
            &TrackId::new(id),
            result.effects(),
            effect_created_at,
        );
        if !enqueued_effects.is_empty() {
            let journal_entries =
                effect_journal_entries_from_enqueued(enqueued_effects.journal_projection_entries());
            if let Err(error) = self.effect_journal.append_entries(&journal_entries).await {
                tracing::warn!(
                    track_id = id,
                    "failed to append effect journal entries: {error}"
                );
            }
        }

        if has_effect_status_update
            && let Err(error) = self
                .effect_journal
                .record_effect_outcomes(effect_status_updates)
                .await
        {
            tracing::warn!(
                track_id = id,
                "failed to record effect journal outcomes: {error}"
            );
        }

        let previous_recovery_anomaly_active = previous_frame.recovery_anomaly_active();
        let next_recovery_anomaly_active = next_frame.recovery_anomaly_active();
        if previous_recovery_anomaly_active != next_recovery_anomaly_active {
            self.recovery_anomaly_observer
                .observe_recovery_anomaly_change(&TrackId::new(id), next_recovery_anomaly_active);
        }

        if has_track_write || has_effect_status_update || has_session_effects {
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

fn cancel_receipt_absorbed_exposure(
    manager: &TrackManager,
    id: &str,
    order_id: &str,
    receipt: &OrderReceipt,
) -> Result<bool> {
    let snapshot = manager
        .mutation_frame(id)
        .ok_or_else(|| anyhow!("track `{id}` not found"))?;
    Ok(snapshot.has_absorbed_binding_for_cancel_receipt(order_id, receipt))
}

fn classify_cancel_receipt_resolution(
    order_id: &str,
    cancel_progressed: bool,
    receipt: &OrderReceipt,
) -> CancelReceiptResolution {
    if cancel_progressed || receipt.filled_qty > f64::EPSILON {
        return CancelReceiptResolution::ClosedWithFill {
            filled_qty: receipt.filled_qty,
        };
    }
    if receipt.status.clears_working_order() {
        return CancelReceiptResolution::ClosedWithoutFill;
    }
    if receipt.status.keeps_working_order() {
        return CancelReceiptResolution::StillWorking;
    }
    CancelReceiptResolution::Unknown {
        order_id: order_id.to_string(),
        reason: format!("unexpected cancel receipt status: {:?}", receipt.status),
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
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{ClockPort, OrderRequest};
    use tokio::sync::broadcast;

    use crate::{
        ApplicationNotification, CommittedTrackWrite, EffectJournalEntry, EffectStatus,
        EffectStatusUpdate, PersistedTrackEffect, TrackControlState, TrackEffectJournal,
        TrackMutationStore,
    };

    use super::{AccountCapacityGuard, TrackServiceSet};

    #[derive(Default)]
    pub(crate) struct NoopGuard;

    impl AccountCapacityGuard for NoopGuard {
        fn available_notional_for(&self, _instrument: &Instrument) -> Option<f64> {
            None
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
        control_states: Mutex<HashMap<String, TrackControlState>>,
        ledger_states: Mutex<HashMap<String, poise_engine::ledger::TrackLedgerState>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedTrackEffect>>,
        business_write_calls: Mutex<usize>,
        effect_outcome_write_calls: Mutex<usize>,
    }

    impl MemoryRepository {
        pub(crate) fn pending_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().unwrap().clone()
        }

        pub(crate) fn effect_outcome_write_call_count(&self) -> usize {
            *self.effect_outcome_write_calls.lock().unwrap()
        }

        pub(crate) fn business_write_call_count(&self) -> usize {
            *self.business_write_calls.lock().unwrap()
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
                    recovery_token: SubmitRecoveryToken::empty(),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
        }

        pub(crate) fn seed_pending_mixed_effect_batch(&self, track_id: &str, batch_id: &str) {
            let now = Utc::now();
            let track_id = TrackId::new(track_id);
            let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
            let effects = [
                PersistedTrackEffect {
                    effect_id: format!("{batch_id}:0"),
                    track_id: track_id.clone(),
                    batch_id: batch_id.to_string(),
                    sequence: 0,
                    effect: TrackEffect::SubmitOrder {
                        request: OrderRequest {
                            instrument: instrument.clone(),
                            side: Side::Buy,
                            price: 100.0,
                            quantity: 0.1,
                            client_order_id: "client-1".into(),
                            reduce_only: false,
                        },
                        desired_exposure: Exposure(4.0),
                        submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                        recovery_token: SubmitRecoveryToken::empty(),
                    },
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                },
                PersistedTrackEffect {
                    effect_id: format!("{batch_id}:1"),
                    track_id,
                    batch_id: batch_id.to_string(),
                    sequence: 1,
                    effect: TrackEffect::CancelAll { instrument },
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                },
            ];
            self.effects.lock().unwrap().extend(effects);
        }

        pub(crate) fn replace_submit_effect_with_cancel_order(
            &self,
            effect_id: &str,
            order_id: &str,
        ) -> PersistedTrackEffect {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .expect("effect should exist");
            let TrackEffect::SubmitOrder { request, .. } = &effect.effect else {
                panic!("effect should be submit order");
            };
            effect.effect = TrackEffect::CancelOrder {
                instrument: request.instrument.clone(),
                order_id: order_id.into(),
            };
            effect.clone()
        }
    }

    #[async_trait]
    impl crate::TrackQueryStore for MemoryRepository {
        async fn list_recent_track_events(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<crate::StoredTrackEvent>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(index, event)| crate::StoredTrackEvent {
                    id: index as i64,
                    track_id: track_id.clone(),
                    event,
                    created_at: Utc::now(),
                })
                .collect())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            limit: usize,
        ) -> Result<Vec<crate::PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .take(limit)
                .cloned()
                .collect())
        }

        async fn load_track_control_state(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<TrackControlState>> {
            Ok(self
                .control_states
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned())
        }

        async fn load_track_ledger_state(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<poise_engine::ledger::TrackLedgerState>> {
            Ok(self
                .ledger_states
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned())
        }

        async fn load_track_updated_at(
            &self,
            _track_id: &TrackId,
        ) -> Result<Option<chrono::DateTime<Utc>>> {
            Ok(None)
        }
    }

    #[async_trait]
    impl TrackMutationStore for MemoryRepository {
        async fn commit_track_transition(
            &self,
            id: &str,
            control_state: Option<&TrackControlState>,
            ledger_state: &poise_engine::ledger::TrackLedgerState,
            events: &[DomainEvent],
        ) -> Result<CommittedTrackWrite> {
            *self.business_write_calls.lock().unwrap() += 1;
            self.events
                .lock()
                .unwrap()
                .insert(id.to_string(), events.to_vec());

            let track_id = TrackId::new(id);
            let persisted_control_state = if control_state.is_none() {
                let has_persisted_control_truth =
                    self.control_states.lock().unwrap().contains_key(id);
                if has_persisted_control_truth {
                    None
                } else {
                    Some(TrackControlState::default())
                }
            } else {
                control_state.cloned()
            };
            if let Some(control_state) = persisted_control_state.as_ref() {
                self.save_track_control_state(&track_id, control_state)
                    .await?;
            }
            self.save_track_ledger_state(&track_id, ledger_state)
                .await?;

            Ok(CommittedTrackWrite { track_id })
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

        async fn save_track_control_state(
            &self,
            track_id: &TrackId,
            state: &TrackControlState,
        ) -> Result<()> {
            self.control_states
                .lock()
                .unwrap()
                .insert(track_id.as_str().to_string(), state.clone());
            Ok(())
        }

        async fn save_track_ledger_state(
            &self,
            track_id: &TrackId,
            state: &poise_engine::ledger::TrackLedgerState,
        ) -> Result<()> {
            self.ledger_states
                .lock()
                .unwrap()
                .insert(track_id.as_str().to_string(), state.clone());
            Ok(())
        }
    }

    #[async_trait]
    impl TrackEffectJournal for MemoryRepository {
        async fn append_entries(&self, entries: &[EffectJournalEntry]) -> Result<()> {
            self.effects
                .lock()
                .unwrap()
                .extend(entries.iter().cloned().map(PersistedTrackEffect::from));
            Ok(())
        }

        async fn record_effect_outcomes(&self, outcomes: &[EffectStatusUpdate]) -> Result<()> {
            if !outcomes.is_empty() {
                *self.effect_outcome_write_calls.lock().unwrap() += 1;
            }
            let now = Utc::now();
            let mut stored_effects = self.effects.lock().unwrap();
            for outcome in outcomes {
                if let Some(effect) = stored_effects
                    .iter_mut()
                    .find(|effect| effect.effect_id == outcome.effect_id)
                {
                    effect.status = outcome.status;
                    effect.attempt_count += outcome.attempt_delta;
                    effect.last_error = outcome.last_error.clone();
                    effect.updated_at = now;
                }
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
                TrackDefinition::try_new(
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
                        out_of_band_policy: BandProtectionPolicy::Freeze,
                    },
                    Some(3_000.0),
                    LossLimits {
                        daily_loss_limit: 300.0,
                        total_loss_limit: 600.0,
                    },
                    None,
                )
                .unwrap(),
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
        seeded_manager()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::session_effect_queue::{SessionPendingEffectState, WakeSignal};
    use crate::{CancelReceiptResolution, EffectStatus, SessionEffectQueue, TrackEffectJournal};
    use poise_core::risk::RiskTerminationCause;
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::{BindingStatus, RecoveryAnomaly};
    use poise_engine::mutation_frame::TrackMutationFrame;
    use poise_engine::observation::{
        CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
    };
    use poise_engine::ports::{ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus};
    use poise_engine::runtime::{AutoState, ControlState, TerminationCause, TrackState};
    use poise_engine::transition::TrackTransition;
    use tokio::sync::broadcast;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::test_support::{MemoryRepository, NoopGuard, seeded_manager, track_write_services};
    use super::{MutationExecutor, RecoveryAnomalyObserver, effect_journal_entries_from_enqueued};
    use crate::{TrackControlState, TrackMutationStore, TrackQueryStore};

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

    fn complete_open_orders(order_ids: &[&str]) -> CompleteOpenOrderSnapshot {
        CompleteOpenOrderSnapshot::from_complete_exchange_query(
            order_ids
                .iter()
                .map(|order_id| OrderObservation {
                    order_id: (*order_id).to_string(),
                    client_order_id: format!("{order_id}-client"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    filled_qty: 0.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                })
                .collect(),
        )
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
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        )
    }

    fn frame_with_recovery_anomaly(active: bool) -> TrackMutationFrame {
        let manager = seeded_manager();
        let mut snapshot = manager.get_track("btc-core").unwrap().mutation_frame();
        snapshot.set_recovery_anomaly(active.then_some(RecoveryAnomaly::UnknownLiveOrder));
        snapshot
    }

    #[tokio::test]
    async fn commit_track_mutation_notifies_recovery_anomaly_activation_edges_only() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer.clone());

        let previous_frame = frame_with_recovery_anomaly(false);
        let mut next_frame = previous_frame.clone();
        next_frame.set_exposure_state(
            poise_core::types::Exposure(1.0),
            previous_frame.desired_exposure().cloned(),
        );

        executor
            .commit_track_mutation(
                "btc-core",
                &previous_frame,
                &next_frame,
                &(),
                None,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(observer.recorded().is_empty());

        let next_frame = frame_with_recovery_anomaly(true);
        executor
            .commit_track_mutation(
                "btc-core",
                &previous_frame,
                &next_frame,
                &(),
                None,
                false,
                None,
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

        let previous_frame = frame_with_recovery_anomaly(true);
        let next_frame = frame_with_recovery_anomaly(false);

        executor
            .commit_track_mutation(
                "btc-core",
                &previous_frame,
                &next_frame,
                &(),
                None,
                false,
                None,
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
            SessionEffectQueue::default(),
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
            let mut updated = track.mutation_frame();
            updated.set_runtime_state(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            )));
            updated.set_exposure_state(
                poise_core::types::Exposure(2.4),
                Some(poise_core::types::Exposure(2.4)),
            );
            manager
                .rollback_track_state(&updated)
                .expect("failed to seed active exposure state");
        }

        let transition = executor
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 97.0,
                        best_ask: 97.0,
                    },
                },
            )
            .await
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_none(),
            "live-only tick should not persist control state"
        );
        assert!(
            repository
                .load_track_ledger_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_none(),
            "live-only tick should not persist any durable runtime projection"
        );
    }

    #[tokio::test]
    async fn complete_effect_succeeded_does_not_create_durable_track_truth() {
        let repository = Arc::new(MemoryRepository::default());
        repository.seed_pending_submit_effect();
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);

        executor
            .complete_effect_succeeded("btc-core", "btc-core:batch-1:0")
            .await
            .unwrap();

        let effects = repository.pending_effects();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Succeeded);
        assert!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_none(),
            "effect status writeback should not create control truth"
        );
        assert!(
            repository
                .load_track_ledger_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_none(),
            "effect status writeback should not create ledger truth"
        );
    }

    #[tokio::test]
    async fn observe_market_first_durable_write_persists_default_automatic_control_state() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);

        let transition = executor
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();

        assert!(
            !transition.effects.is_empty() || !transition.events.is_empty(),
            "test requires a durable transition"
        );
        assert_eq!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap(),
            Some(TrackControlState::default())
        );
        assert!(
            repository
                .load_track_ledger_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_some(),
            "first durable write should also establish ledger truth"
        );
    }

    #[tokio::test]
    async fn observe_market_enqueues_current_session_effects() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);

        let transition = services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();

        assert!(
            !transition.effects.is_empty(),
            "test requires market observation to generate executable effects"
        );
        let next = services
            .session_effect_queue
            .claim_next()
            .expect("current session effect should be enqueued");

        assert_eq!(next.track_id, TrackId::new("btc-core"));
    }

    #[tokio::test]
    async fn effect_only_transition_enqueues_effect_without_business_write() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);
        let snapshot = executor
            .manager()
            .read()
            .await
            .mutation_frame("btc-core")
            .expect("seeded track should exist");
        let effect = TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 95.0,
                quantity: 0.1,
                client_order_id: "effect-only-submit".into(),
                reduce_only: false,
            },
            desired_exposure: Exposure(4.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
        };
        let transition = TrackTransition {
            frame: snapshot.clone(),
            events: Vec::new(),
            effects: vec![effect],
        };

        executor
            .commit_track_mutation(
                "btc-core",
                &snapshot,
                &snapshot,
                &transition,
                None,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            repository.business_write_call_count(),
            0,
            "session effect enqueue should not create a durable business write"
        );
        assert_eq!(repository.pending_effects().len(), 1);
        assert!(
            executor.session_effect_queue.claim_next().is_some(),
            "effect should still enter the current session queue"
        );
    }

    #[tokio::test]
    async fn exchange_sync_records_cancel_follow_up_outcomes() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        let track_id = TrackId::new("btc-core");
        let now = chrono::Utc::now();
        let enqueued = services.session_effect_queue.enqueue_transition_effects(
            &track_id,
            &[
                TrackEffect::CancelOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "closed-order".into(),
                },
                TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 0.1,
                        client_order_id: "downstream-submit".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                    recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
                },
            ],
            now,
        );
        let effect_ids = enqueued.effect_ids();
        let cancel_effect_id = effect_ids[0].clone();
        let journal_entries =
            effect_journal_entries_from_enqueued(enqueued.journal_projection_entries());
        repository.append_entries(&journal_entries).await.unwrap();
        services.session_effect_queue.record_cancel_resolution(
            &cancel_effect_id,
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "exchange timeout".into(),
            },
        );

        services
            .observation
            .sync_exchange_state_without_reconcile(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        let effects = repository.pending_effects();
        let downstream = effects
            .iter()
            .find(|effect| effect.effect_id == effect_ids[1])
            .expect("downstream journal row should exist");
        assert_eq!(downstream.status, EffectStatus::Superseded);
    }

    #[tokio::test]
    async fn cancel_follow_up_resolution_waits_for_successful_exchange_sync_commit() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        let track_id = TrackId::new("ghost-core");
        let enqueued = services.session_effect_queue.enqueue_transition_effects(
            &track_id,
            &[
                TrackEffect::CancelOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "closed-order".into(),
                },
                TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 0.1,
                        client_order_id: "downstream-submit".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                    recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
                },
            ],
            chrono::Utc::now(),
        );
        let cancel_effect_id = enqueued.effect_ids()[0].clone();
        services.session_effect_queue.claim_next().unwrap();
        services.session_effect_queue.record_cancel_resolution(
            &cancel_effect_id,
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "exchange timeout".into(),
            },
        );

        let result = services
            .observation
            .sync_exchange_state_without_reconcile(
                track_id.as_str(),
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await;

        assert!(result.is_err());
        let snapshot = services.session_effect_queue.snapshot_for_track(&track_id);
        assert_eq!(snapshot.pending_effects.len(), 2);
        assert_eq!(
            snapshot.pending_effects[0].state,
            SessionPendingEffectState::AwaitingFollowUp,
            "failed exchange sync must not consume the pending cancel follow-up"
        );
    }

    #[tokio::test]
    async fn closed_cancel_follow_up_forces_reconcile_even_from_recover_only_sync() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());
        let track_id = TrackId::new("btc-core");
        services
            .observation
            .observe_market(
                track_id.as_str(),
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .await
            .unwrap();
        services.session_effect_queue.clear_track(&track_id);

        let enqueued = services.session_effect_queue.enqueue_transition_effects(
            &track_id,
            &[TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: "closed-order".into(),
            }],
            chrono::Utc::now(),
        );
        let cancel_effect_id = enqueued.effect_ids()[0].clone();
        services.session_effect_queue.claim_next().unwrap();
        services.session_effect_queue.record_cancel_resolution(
            &cancel_effect_id,
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "exchange timeout".into(),
            },
        );

        let transition = services
            .observation
            .sync_exchange_state_without_reconcile(
                track_id.as_str(),
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        assert!(
            !transition.effects.is_empty(),
            "closed cancel follow-up should force reconcile even through recover-only exchange sync"
        );
    }

    #[tokio::test]
    async fn still_open_cancel_follow_up_consumes_current_exchange_state_wake() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        let track_id = TrackId::new("btc-core");
        let enqueued = services.session_effect_queue.enqueue_transition_effects(
            &track_id,
            &[
                TrackEffect::CancelOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "open-order".into(),
                },
                TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 0.1,
                        client_order_id: "downstream-submit".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                    recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
                },
            ],
            chrono::Utc::now(),
        );
        let cancel_effect_id = enqueued.effect_ids()[0].clone();
        assert_eq!(
            services
                .session_effect_queue
                .claim_next()
                .expect("cancel should be claimable")
                .effect_id,
            cancel_effect_id
        );
        services.session_effect_queue.record_cancel_resolution(
            &cancel_effect_id,
            CancelReceiptResolution::Unknown {
                order_id: "open-order".into(),
                reason: "exchange timeout".into(),
            },
        );

        services
            .observation
            .sync_exchange_state_without_reconcile(
                "btc-core",
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                complete_open_orders(&["open-order"]),
            )
            .await
            .unwrap();

        assert!(
            services.session_effect_queue.claim_next().is_none(),
            "the exchange-state wake represented by this sync was consumed by the still-open follow-up"
        );

        services
            .session_effect_queue
            .wake_track_for(&track_id, WakeSignal::ExchangeState);
        assert_eq!(
            services
                .session_effect_queue
                .claim_next()
                .expect("a later exchange-state wake should retry the original cancel")
                .effect_id,
            cancel_effect_id
        );
    }

    #[tokio::test]
    async fn mutation_store_backfills_default_control_state_on_first_durable_write() {
        let repository = Arc::new(MemoryRepository::default());
        repository
            .commit_track_transition(
                "btc-core",
                None,
                &poise_engine::ledger::TrackLedgerState::default(),
                &[],
            )
            .await
            .expect("store owner should complete durable truth on first business write");
        assert_eq!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap(),
            Some(TrackControlState::default())
        );
    }

    #[tokio::test]
    async fn observe_market_persists_automatic_risk_termination_control_state() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);

        {
            let manager = executor.manager();
            let mut manager = manager.write().await;
            let track = manager
                .get_track("btc-core")
                .cloned()
                .expect("seeded track should exist");
            let mut updated = track.mutation_frame();
            updated.set_runtime_state(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            )));
            updated.set_exposure_state(poise_core::types::Exposure(4.0), None);
            let mut ledger_state = updated.ledger_state().clone();
            ledger_state.gross_realized_pnl_today = -290.0;
            ledger_state.gross_realized_pnl_cumulative = -290.0;
            updated.replace_ledger_state(ledger_state);
            updated.set_unrealized_pnl(-20.0);
            manager
                .rollback_track_state(&updated)
                .expect("failed to seed risk termination state");
        }

        let transition = executor
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 95.0,
                        best_ask: 95.0,
                    },
                },
            )
            .await
            .unwrap();

        assert_eq!(
            transition.frame.runtime_state(),
            &TrackState::Terminated {
                cause: TerminationCause::Risk(RiskTerminationCause::DailyLossLimit),
            }
        );
        assert_eq!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap(),
            Some(TrackControlState::Terminated {
                cause: TerminationCause::Risk(RiskTerminationCause::DailyLossLimit),
            })
        );
    }

    #[tokio::test]
    async fn record_cancel_order_success_clears_cancel_binding_and_preserves_downstream_submits() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 104.9,
                        best_ask: 105.1,
                    },
                },
            )
            .await
            .unwrap();

        let mut pending_submits = repository
            .pending_effects()
            .into_iter()
            .filter_map(|effect| match effect.effect {
                TrackEffect::SubmitOrder { .. } => Some(effect),
                _ => None,
            })
            .collect::<Vec<_>>();
        pending_submits.sort_by_key(|effect| effect.sequence);
        assert!(
            pending_submits.len() > 2,
            "test requires multiple downstream submit effects"
        );
        let blocked_submit = pending_submits[0].clone();
        let TrackEffect::SubmitOrder {
            request: blocked_request,
            ..
        } = &blocked_submit.effect
        else {
            panic!("blocked effect should be submit order");
        };
        let blocked_client_order_id = blocked_request.client_order_id.clone();
        let downstream_client_order_ids = pending_submits
            .iter()
            .filter(|effect| {
                effect.batch_id == blocked_submit.batch_id
                    && effect.sequence > blocked_submit.sequence
            })
            .filter_map(|effect| match &effect.effect {
                TrackEffect::SubmitOrder { request, .. } => Some(request.client_order_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            downstream_client_order_ids.len() > 1,
            "test requires more than one downstream submit after the cancel effect"
        );
        let closed_order_id = "closed-order";
        let cancel_effect = repository
            .replace_submit_effect_with_cancel_order(&blocked_submit.effect_id, closed_order_id);

        let manager = services.effect.manager();
        {
            let mut manager = manager.write().await;
            let mut snapshot = manager
                .mutation_frame("btc-core")
                .expect("track mutation frame");
            assert!(
                snapshot.set_binding_order_status_for_client_order_id(
                    &blocked_client_order_id,
                    Some(closed_order_id.into()),
                    BindingStatus::CancelPending,
                ),
                "blocked submit binding should exist"
            );
            manager
                .rollback_track_state(&snapshot)
                .expect("updated track mutation frame");
        }
        let resolution = services
            .effect
            .record_cancel_order_success(
                "btc-core",
                &cancel_effect.effect_id,
                "closed-order",
                &OrderReceipt {
                    order_id: "closed-order".to_string(),
                    client_order_id: blocked_client_order_id.clone(),
                    filled_qty: 0.0,
                    status: poise_engine::ports::OrderStatus::Canceled,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            resolution,
            CancelReceiptResolution::ClosedWithoutFill,
            "no-fill cancel should let the session queue release downstream submits"
        );

        let persisted = repository.pending_effects();
        let persisted_cancel = persisted
            .iter()
            .find(|effect| effect.effect_id == cancel_effect.effect_id)
            .expect("cancel effect should remain persisted");
        assert_eq!(persisted_cancel.status, EffectStatus::Succeeded);

        let snapshot = manager
            .read()
            .await
            .mutation_frame("btc-core")
            .expect("track mutation frame");
        let (_, blocked_binding_status) = snapshot
            .binding_receipt_for_client_order_id(&blocked_client_order_id)
            .expect("blocked binding should remain in runtime history");
        assert_eq!(blocked_binding_status, BindingStatus::Terminal);
        assert!(
            !snapshot.has_active_binding_for_order_id(closed_order_id),
            "closed order should no longer have an active runtime binding"
        );
        for client_order_id in downstream_client_order_ids {
            assert_eq!(
                snapshot.binding_is_active_for_client_order_id(&client_order_id),
                Some(true),
                "downstream binding should remain in runtime state"
            );
        }
    }

    #[tokio::test]
    async fn record_cancel_order_success_with_fill_classifies_downstream_queue_action() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .observation
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 104.9,
                        best_ask: 105.1,
                    },
                },
            )
            .await
            .unwrap();

        let mut pending_submits = repository
            .pending_effects()
            .into_iter()
            .filter_map(|effect| match effect.effect {
                TrackEffect::SubmitOrder { .. } => Some(effect),
                _ => None,
            })
            .collect::<Vec<_>>();
        pending_submits.sort_by_key(|effect| effect.sequence);
        assert!(
            pending_submits.len() > 2,
            "test requires multiple downstream submit effects"
        );
        let blocked_submit = pending_submits[0].clone();
        let TrackEffect::SubmitOrder {
            request: blocked_request,
            ..
        } = &blocked_submit.effect
        else {
            panic!("blocked effect should be submit order");
        };
        let blocked_client_order_id = blocked_request.client_order_id.clone();
        let downstream = pending_submits
            .iter()
            .filter(|effect| {
                effect.batch_id == blocked_submit.batch_id
                    && effect.sequence > blocked_submit.sequence
            })
            .filter_map(|effect| match &effect.effect {
                TrackEffect::SubmitOrder { request, .. } => {
                    Some((effect.effect_id.clone(), request.client_order_id.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            downstream.len() > 1,
            "test requires more than one downstream submit after the cancel effect"
        );
        let closed_order_id = "closed-order";
        let cancel_effect = repository
            .replace_submit_effect_with_cancel_order(&blocked_submit.effect_id, closed_order_id);

        let manager = services.effect.manager();
        {
            let mut manager = manager.write().await;
            let mut snapshot = manager
                .mutation_frame("btc-core")
                .expect("track mutation frame");
            assert!(
                snapshot.set_binding_order_status_for_client_order_id(
                    &blocked_client_order_id,
                    Some(closed_order_id.into()),
                    BindingStatus::CancelPending,
                ),
                "blocked submit binding should exist"
            );
            manager
                .rollback_track_state(&snapshot)
                .expect("updated track mutation frame");
        }
        let outcome_writes_before_cancel = repository.effect_outcome_write_call_count();

        let resolution = services
            .effect
            .record_cancel_order_success(
                "btc-core",
                &cancel_effect.effect_id,
                "closed-order",
                &OrderReceipt {
                    order_id: "closed-order".to_string(),
                    client_order_id: blocked_client_order_id.clone(),
                    filled_qty: 0.05,
                    status: poise_engine::ports::OrderStatus::Canceled,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            resolution,
            CancelReceiptResolution::ClosedWithFill { filled_qty: 0.05 },
            "cancel with fill should let the session queue retire downstream submits"
        );

        let persisted = repository.pending_effects();
        let persisted_cancel = persisted
            .iter()
            .find(|effect| effect.effect_id == cancel_effect.effect_id)
            .expect("cancel effect should remain persisted");
        assert_eq!(persisted_cancel.status, EffectStatus::Succeeded);
        for (effect_id, _) in &downstream {
            let effect = persisted
                .iter()
                .find(|effect| effect.effect_id == *effect_id)
                .expect("downstream submit effect should remain persisted");
            assert_eq!(
                effect.status,
                EffectStatus::Pending,
                "mutation writeback only classifies the receipt; queue owns downstream retirement"
            );
        }
        assert_eq!(
            repository.effect_outcome_write_call_count(),
            outcome_writes_before_cancel + 1,
            "cancel-with-fill should record one diagnostic outcome batch for the cancel receipt"
        );

        let snapshot = manager
            .read()
            .await
            .mutation_frame("btc-core")
            .expect("track mutation frame");
        for (_, client_order_id) in downstream {
            assert!(
                snapshot.binding_is_active_for_client_order_id(&client_order_id) == Some(true),
                "queue action, not cancel receipt writeback, retires downstream runtime bindings"
            );
        }
    }
}
