use std::collections::HashMap;
use std::sync::Arc;

use crate::configuration_application::{
    ConfigurationApplicationPersistence, ConfigurationApplicationRequest,
    ConfigurationApplicationStatus, ConfigurationApplicationStore,
    ConfigurationApplicationStoreError,
};
use crate::exchange::{OrderReceipt, OrderRequest};
use crate::session_effect_queue::{
    EnqueuedEffectJournalEntry, FollowUpQueueAction, SessionExchangeStateRevision, WakeSignal,
};
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
use poise_engine::ledger::{TrackPnlRecord, TrackPnlStats};
use poise_engine::manager::{ExchangeSyncMode, SubmitRecoveryOrderLookup, TrackManager};
use poise_engine::observation::{
    CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
};
use poise_engine::runtime::{
    ExchangeObservationRevision, FreshSessionExternalInputs, QuoteHealthView, StrategyTargetView,
    TerminationCause, TrackLiveView, TrackRuntime, TrackRuntimeView, TrackState,
};
use poise_engine::transition::{CommitIntent, TrackTransition};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, broadcast};

pub trait AccountCapacityGuard: Send + Sync {
    fn available_notional_for(&self, instrument: &Instrument) -> Option<f64>;
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

struct OwnedTrackMutationCommit {
    id: String,
    previous_state: TrackRuntime,
    next_state: TrackRuntime,
    domain_events: Vec<DomainEvent>,
    effects: Vec<TrackEffect>,
    effect_status_updates: Vec<EffectStatusUpdate>,
    skip_when_no_work: bool,
    control_state_override: Option<TrackControlState>,
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
pub struct TradingRuntime {
    manager: SharedManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_journal: Arc<dyn TrackEffectJournal>,
    effect_journal_diagnostics: Arc<crate::EffectJournalDiagnostics>,
    session_effect_queue: SessionEffectQueue,
    mutation_guards: Arc<TrackMutationGuards>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<dyn AccountCapacityGuard>,
    recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    pending_configuration_applications:
        Arc<Mutex<HashMap<TrackId, PendingConfigurationApplication>>>,
}

#[derive(Clone)]
struct PendingConfigurationApplication {
    request: ConfigurationApplicationRequest,
    store: Arc<dyn ConfigurationApplicationStore>,
    /// The definition that remains authoritative until the candidate file has
    /// been replaced.  It is also the definition restored after a discarded
    /// pre-write application has crossed the terminal-order barrier.
    previous_definition: poise_core::track::TrackDefinition,
    file_replaced: bool,
    abandoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInstrument {
    pub id: String,
    pub instrument: Instrument,
}

#[derive(Debug, Clone)]
pub(crate) struct CommittedTrackSnapshot {
    pub definition: poise_core::track::TrackDefinition,
    pub runtime: TrackRuntimeView,
}

#[derive(Debug, Clone)]
pub enum TrackInput {
    Command(TrackCommand),
    Market {
        observation: MarketObservation,
        occurred_at: chrono::DateTime<chrono::Utc>,
    },
    RefreshMarketDataHealth,
    Position(PositionObservation),
    Order(OrderObservation),
    TrackPnl(TrackPnlRecord),
    DurableEvents(Vec<DomainEvent>),
    PrepareFreshSession,
    StartFreshSession {
        control_state: TrackControlState,
        pnl_stats: TrackPnlStats,
        external_inputs: FreshSessionExternalInputs,
    },
    SyncExchangeState {
        token: ExchangeStateSyncToken,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
        reconcile: bool,
    },
    EffectsSuperseded {
        effect_ids: Vec<String>,
    },
    SubmitOutcomeUnknown {
        effect_id: String,
        client_order_id: String,
        initial_snapshot_confirmed: bool,
    },
    SubmitCompleted {
        effect_id: String,
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: OrderReceipt,
    },
    SubmitFailed {
        effect_id: String,
        client_order_id: String,
        error: String,
    },
    SubmitEffectFailed {
        effect_id: String,
        error: String,
    },
    RecoverSubmit {
        effect_id: String,
        recovery_token: SubmitRecoveryToken,
        order_lookup: SubmitRecoveryOrderLookup,
    },
    CancelOrderSucceeded {
        effect_id: String,
        order_id: String,
        receipt: OrderReceipt,
    },
    CancelAllSucceeded {
        effect_id: String,
    },
    EffectSucceeded {
        effect_id: String,
    },
    EffectFailed {
        effect_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitRecoveryReceipt {
    Dispatch {
        request: OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    },
    Finished,
}

#[derive(Debug, Clone)]
pub enum TrackUpdateReceipt {
    Committed {
        decision_revision: Option<u64>,
    },
    OrderObserved {
        absorb_result: OrderUpdateAbsorbResult,
    },
    PnlRecorded {
        inserted: bool,
    },
    FreshSessionStarted {
        track_found: bool,
    },
    ExchangeStateSynced(ExchangeStateSyncOutcome),
    SubmitOutcomeUnknownRecorded {
        changed: bool,
    },
    SubmitAttemptFinished,
    SubmitRecovery(SubmitRecoveryReceipt),
    CancelOrderResolved(CancelReceiptResolution),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeStateSyncToken {
    track_id: TrackId,
    observation_revision: ExchangeObservationRevision,
    queue_revision: SessionExchangeStateRevision,
}

#[derive(Debug, Clone)]
pub enum ExchangeStateSyncOutcome {
    Applied,
    RevisionChanged,
}

#[derive(Debug)]
pub enum TrackMutationError {
    LoadedTrackInvariant { track_id: String },
    Mutation(anyhow::Error),
    Persistence(anyhow::Error),
}

#[derive(Debug)]
pub enum ConfigurationApplicationError {
    TrackNotLoaded { track_id: String },
    Terminated,
    RecoveryAnomaly,
    TrackIdentityMismatch,
    AlreadyPersistedNotInstalled,
    PersistedApplicationCannotBeAbandoned,
    ApplicationIdMismatch,
    Persistence(ConfigurationApplicationStoreError),
    ApplicationInProgress,
}

impl std::fmt::Display for ConfigurationApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ConfigurationApplicationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationApplicationReceipt {
    pub status: ConfigurationApplicationStatus,
    pub persistence: ConfigurationApplicationPersistence,
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
        &self.facts
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

impl TradingRuntime {
    pub fn new(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_journal: Arc<dyn TrackEffectJournal>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
    ) -> Self {
        Self::new_with_recovery_anomaly_observer(
            manager,
            mutation_store,
            effect_journal,
            notifications,
            account_margin_guard,
            Arc::new(NoopRecoveryAnomalyObserver),
        )
    }

    pub fn new_with_recovery_anomaly_observer(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_journal: Arc<dyn TrackEffectJournal>,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        Self::with_session_queue(
            manager,
            mutation_store,
            effect_journal,
            SessionEffectQueue::default(),
            notifications,
            account_margin_guard,
            recovery_anomaly_observer,
        )
    }

    pub(crate) fn with_session_queue(
        manager: TrackManager,
        mutation_store: Arc<dyn TrackMutationStore>,
        effect_journal: Arc<dyn TrackEffectJournal>,
        session_effect_queue: SessionEffectQueue,
        notifications: broadcast::Sender<ApplicationNotification>,
        account_margin_guard: Arc<dyn AccountCapacityGuard>,
        recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
    ) -> Self {
        let effect_journal_diagnostics = Arc::new(crate::EffectJournalDiagnostics::new());
        Self {
            manager: Arc::new(RwLock::new(manager)),
            mutation_store,
            effect_journal,
            effect_journal_diagnostics,
            session_effect_queue,
            mutation_guards: Arc::new(TrackMutationGuards::default()),
            notifications,
            account_margin_guard,
            recovery_anomaly_observer,
            pending_configuration_applications: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn session_effect_queue(&self) -> SessionEffectQueue {
        self.session_effect_queue.clone()
    }

    /// Applies the no-binding fast path. It deliberately does not cancel or clear
    /// bindings: any outstanding ownership must be resolved by the normal flow.
    pub async fn apply_configuration_application(
        &self,
        track_id: &TrackId,
        request: ConfigurationApplicationRequest,
        store: Arc<dyn ConfigurationApplicationStore>,
    ) -> std::result::Result<ConfigurationApplicationReceipt, ConfigurationApplicationError> {
        let existing_pending = {
            let pending = self.pending_configuration_applications.lock().await;
            pending.get(track_id).cloned()
        };
        if let Some(existing) = existing_pending {
            if existing.request.application_id != request.application_id
                || existing.request.request_hash != request.request_hash
            {
                return Err(ConfigurationApplicationError::ApplicationInProgress);
            }
            let status = if existing.abandoned {
                ConfigurationApplicationStatus::WaitingForOrders
            } else if existing.file_replaced {
                ConfigurationApplicationStatus::Activating
            } else {
                ConfigurationApplicationStatus::WaitingForOrders
            };
            existing
                .store
                .update_configuration_application_status(track_id, &existing.request, status)
                .await
                .map_err(ConfigurationApplicationError::Persistence)?;
            let installed = self
                .manager
                .read()
                .await
                .get_track(track_id.as_str())
                .is_some_and(|track| track.definition() == &existing.request.candidate);
            if existing.file_replaced && installed {
                existing
                    .store
                    .update_configuration_application_status(
                        track_id,
                        &existing.request,
                        ConfigurationApplicationStatus::Active,
                    )
                    .await
                    .map_err(ConfigurationApplicationError::Persistence)?;
                let receipt = ConfigurationApplicationReceipt {
                    status: ConfigurationApplicationStatus::Active,
                    persistence: ConfigurationApplicationPersistence::AlreadyPersisted,
                };
                self.pending_configuration_applications
                    .lock()
                    .await
                    .remove(track_id);
                return Ok(receipt);
            }
            return Ok(ConfigurationApplicationReceipt {
                status,
                persistence: ConfigurationApplicationPersistence::AlreadyPersisted,
            });
        }
        {
            let pending = self.pending_configuration_applications.lock().await;
            if !pending.is_empty() {
                return Err(ConfigurationApplicationError::ApplicationInProgress);
            }
        }
        let guard = self.lock_track_mutation(track_id.as_str()).await;
        // A concurrent same-ID request can pass the optimistic check above
        // while this call waits for the per-track mutation guard.  Recheck
        // pending work under that serialization point before durable audit.
        if let Some(existing) = self
            .pending_configuration_applications
            .lock()
            .await
            .get(track_id)
            .cloned()
        {
            if existing.request.application_id == request.application_id
                && existing.request.request_hash == request.request_hash
            {
                drop(guard);
                return Ok(ConfigurationApplicationReceipt {
                    status: if existing.file_replaced {
                        ConfigurationApplicationStatus::Activating
                    } else {
                        ConfigurationApplicationStatus::WaitingForOrders
                    },
                    persistence: ConfigurationApplicationPersistence::AlreadyPersisted,
                });
            }
            drop(guard);
            return Err(ConfigurationApplicationError::ApplicationInProgress);
        }
        let previous_state = {
            let manager = self.manager.read().await;
            let previous_state =
                manager
                    .get_track(track_id.as_str())
                    .cloned()
                    .ok_or_else(|| ConfigurationApplicationError::TrackNotLoaded {
                        track_id: track_id.as_str().to_string(),
                    })?;
            if previous_state.runtime_state().is_terminated() {
                return Err(ConfigurationApplicationError::Terminated);
            }
            if previous_state.recovery_anomaly_active() {
                return Err(ConfigurationApplicationError::RecoveryAnomaly);
            }
            if request.candidate.track_id() != previous_state.id()
                || request.candidate.instrument() != previous_state.instrument()
            {
                return Err(ConfigurationApplicationError::TrackIdentityMismatch);
            }
            previous_state
        };
        // This is a local durable audit write, deliberately completed before
        // any cancellation effect is committed.
        let acceptance = store
            .accept_configuration_application(track_id, &request)
            .await
            .map_err(ConfigurationApplicationError::Persistence)?;
        if acceptance == ConfigurationApplicationPersistence::AlreadyPersisted {
            if previous_state.definition() == &request.candidate {
                store
                    .update_configuration_application_status(
                        track_id,
                        &request,
                        ConfigurationApplicationStatus::Active,
                    )
                    .await
                    .map_err(ConfigurationApplicationError::Persistence)?;
                self.emit_internal_notification(ApplicationNotification::TrackChanged {
                    track_id: track_id.clone(),
                });
                let receipt = ConfigurationApplicationReceipt {
                    status: ConfigurationApplicationStatus::Active,
                    persistence: acceptance,
                };
                return Ok(receipt);
            }
            let _ = store
                .update_configuration_application_status(
                    track_id,
                    &request,
                    ConfigurationApplicationStatus::Failed,
                )
                .await;
            return Err(ConfigurationApplicationError::AlreadyPersistedNotInstalled);
        }
        let can_activate_without_order_barrier = !previous_state.has_active_executor_bindings()
            && !self
                .session_effect_queue
                .has_unresolved_submit_outcome_for_track(track_id);
        let transition = {
            let manager = self.manager.read().await;
            let planned = manager
                .plan_track_update(track_id, |candidate_manager| {
                    candidate_manager.configuration_application_wait_transition(track_id)
                })
                .map_err(|_| ConfigurationApplicationError::TrackIdentityMismatch)?;
            let (transition, _) = planned.into_parts();
            transition.map_err(|_| ConfigurationApplicationError::TrackIdentityMismatch)?
        };
        let install_guard = self
            .commit_planned_track_mutation(
                guard,
                track_id.as_str(),
                &previous_state,
                transition.next_state(),
                &transition,
                None,
                false,
                None,
            )
            .await
            .map_err(|error| {
                ConfigurationApplicationError::Persistence(
                    ConfigurationApplicationStoreError::Internal(error.to_string()),
                )
            })?;
        // Do not remove queued effects until the closed submit gate itself is
        // committed.  A failed pre-write state commit must leave the old
        // runtime and its ownership facts untouched.
        self.pending_configuration_applications.lock().await.insert(
            track_id.clone(),
            PendingConfigurationApplication {
                request: request.clone(),
                store: Arc::clone(&store),
                previous_definition: previous_state.definition().clone(),
                file_replaced: false,
                abandoned: false,
            },
        );
        let superseded_submit_effect_ids = self
            .session_effect_queue
            .supersede_queued_submit_effects_for_track(track_id);
        self.record_effects_superseded(track_id.as_str(), &superseded_submit_effect_ids)
            .await
            .map_err(|error| {
                ConfigurationApplicationError::Persistence(
                    ConfigurationApplicationStoreError::Internal(error.to_string()),
                )
            })?;
        if let Err(error) = store
            .update_configuration_application_status(
                track_id,
                &request,
                ConfigurationApplicationStatus::WaitingForOrders,
            )
            .await
        {
            return Err(ConfigurationApplicationError::Persistence(error));
        }
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: track_id.clone(),
        });
        if can_activate_without_order_barrier {
            // `commit_planned_track_mutation` returns the per-track guard so
            // callers can batch work.  Activation reacquires that guard.
            drop(install_guard);
            let persistence = self
                .advance_configuration_application_after_snapshot(track_id)
                .await
                .map_err(|error| {
                    ConfigurationApplicationError::Persistence(
                        error
                            .downcast_ref::<ConfigurationApplicationStoreError>()
                            .cloned()
                            .unwrap_or_else(|| {
                                ConfigurationApplicationStoreError::Internal(error.to_string())
                            }),
                    )
                })?
                .expect("an order-free configuration application must activate");
            return Ok(ConfigurationApplicationReceipt {
                status: ConfigurationApplicationStatus::Active,
                persistence,
            });
        }
        drop(install_guard);
        Ok(ConfigurationApplicationReceipt {
            status: ConfigurationApplicationStatus::WaitingForOrders,
            persistence: acceptance,
        })
    }

    /// Discards an accepted application only while its candidate has not been
    /// written.  The existing closed submit gate remains in force until a
    /// complete exchange snapshot proves that the old order ownership is
    /// resolved; then the old definition is reactivated through the normal
    /// decision path.
    pub async fn abandon_configuration_application(
        &self,
        track_id: &TrackId,
        application_id: &str,
        store: Arc<dyn ConfigurationApplicationStore>,
    ) -> std::result::Result<ConfigurationApplicationReceipt, ConfigurationApplicationError> {
        let request = {
            let mut pending = self.pending_configuration_applications.lock().await;
            let Some(session) = pending.get_mut(track_id) else {
                return Err(ConfigurationApplicationError::ApplicationInProgress);
            };
            if session.request.application_id != application_id {
                return Err(ConfigurationApplicationError::ApplicationIdMismatch);
            }
            if session.file_replaced {
                return Err(ConfigurationApplicationError::PersistedApplicationCannotBeAbandoned);
            }
            session.abandoned = true;
            session.request.clone()
        };
        store
            .update_configuration_application_status(
                track_id,
                &request,
                ConfigurationApplicationStatus::WaitingForOrders,
            )
            .await
            .map_err(ConfigurationApplicationError::Persistence)?;
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: track_id.clone(),
        });
        Ok(ConfigurationApplicationReceipt {
            status: ConfigurationApplicationStatus::WaitingForOrders,
            persistence: ConfigurationApplicationPersistence::AlreadyPersisted,
        })
    }

    /// Advances the accepted application only after the caller has applied a
    /// `CompleteOpenOrderSnapshot` through `TrackInput::SyncExchangeState`.
    /// A cancel acknowledgement deliberately cannot call this method.
    async fn advance_configuration_application_after_snapshot(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<ConfigurationApplicationPersistence>> {
        let pending = {
            let sessions = self.pending_configuration_applications.lock().await;
            let Some(session) = sessions.get(track_id) else {
                return Ok(None);
            };
            session.clone()
        };
        if self
            .session_effect_queue
            .has_unresolved_submit_outcome_for_track(track_id)
        {
            return Ok(None);
        }
        {
            let manager = self.manager.read().await;
            let track = manager
                .get_track(track_id.as_str())
                .ok_or_else(|| anyhow!("track `{}` not found", track_id.as_str()))?;
            if track.has_active_executor_bindings() || track.recovery_anomaly_active() {
                return Ok(None);
            }
        }
        if pending.file_replaced {
            anyhow::bail!(
                "configuration file was replaced but runtime installation did not complete"
            );
        }
        if pending.abandoned {
            self.restore_previous_configuration_after_barrier(track_id, &pending)
                .await?;
            if let Err(error) = pending
                .store
                .update_configuration_application_status(
                    track_id,
                    &pending.request,
                    ConfigurationApplicationStatus::Failed,
                )
                .await
            {
                self.reclose_configuration_application_gate(track_id)
                    .await?;
                return Err(anyhow::Error::new(error));
            }
            self.pending_configuration_applications
                .lock()
                .await
                .remove(track_id);
            return Ok(None);
        }
        pending
            .store
            .update_configuration_application_status(
                track_id,
                &pending.request,
                ConfigurationApplicationStatus::Activating,
            )
            .await
            .map_err(anyhow::Error::new)?;
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: track_id.clone(),
        });
        // The candidate is written only after the terminal-order barrier, and
        // not while the mutation guard is held across the storage operation.
        let persistence = match pending
            .store
            .persist_accepted_configuration(track_id, &pending.request)
            .await
        {
            Ok(persistence) => persistence,
            Err(
                error @ ConfigurationApplicationStoreError::FileReplacedButDurabilityUncertain(_),
            ) => {
                self.pending_configuration_applications
                    .lock()
                    .await
                    .get_mut(track_id)
                    .ok_or_else(|| anyhow!("configuration application disappeared"))?
                    .file_replaced = true;
                return Err(anyhow::Error::new(error));
            }
            Err(error) => {
                // The complete snapshot barrier above proves that no old
                // order ownership remains.  Because the file was not
                // replaced, it is now safe to reopen the old definition.
                self.restore_previous_configuration_after_barrier(track_id, &pending)
                    .await?;
                if pending
                    .store
                    .update_configuration_application_status(
                        track_id,
                        &pending.request,
                        ConfigurationApplicationStatus::Failed,
                    )
                    .await
                    .is_ok()
                {
                    self.pending_configuration_applications
                        .lock()
                        .await
                        .remove(track_id);
                } else {
                    self.reclose_configuration_application_gate(track_id)
                        .await?;
                }
                return Err(anyhow::Error::new(error));
            }
        };
        self.pending_configuration_applications
            .lock()
            .await
            .get_mut(track_id)
            .ok_or_else(|| anyhow!("configuration application disappeared"))?
            .file_replaced = true;
        let guard = self.lock_track_mutation(track_id.as_str()).await;
        let (previous_state, transition) = {
            let manager = self.manager.read().await;
            let previous_state = manager
                .get_track(track_id.as_str())
                .cloned()
                .ok_or_else(|| anyhow!("track `{}` not found", track_id.as_str()))?;
            anyhow::ensure!(
                !previous_state.has_active_executor_bindings()
                    && !previous_state.recovery_anomaly_active(),
                "configuration application terminal-order barrier is no longer valid"
            );
            let planned = manager.plan_track_update(track_id, |candidate_manager| {
                candidate_manager
                    .configuration_activation_transition(
                        track_id,
                        pending.request.candidate.clone(),
                    )
                    .map_err(anyhow::Error::new)
            })?;
            let (transition, _) = planned.into_parts();
            (previous_state, transition?)
        };
        if let Err(error) = self
            .commit_planned_track_mutation(
                guard,
                track_id.as_str(),
                &previous_state,
                transition.next_state(),
                &transition,
                None,
                false,
                None,
            )
            .await
        {
            return Err(anyhow::Error::new(error));
        }
        pending
            .store
            .update_configuration_application_status(
                track_id,
                &pending.request,
                ConfigurationApplicationStatus::Active,
            )
            .await
            .map_err(anyhow::Error::new)?;
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: track_id.clone(),
        });
        self.pending_configuration_applications
            .lock()
            .await
            .remove(track_id);
        Ok(Some(persistence))
    }

    async fn restore_previous_configuration_after_barrier(
        &self,
        track_id: &TrackId,
        pending: &PendingConfigurationApplication,
    ) -> Result<()> {
        let guard = self.lock_track_mutation(track_id.as_str()).await;
        let (previous_state, transition) = {
            let manager = self.manager.read().await;
            let previous_state = manager
                .get_track(track_id.as_str())
                .cloned()
                .ok_or_else(|| anyhow!("track `{}` not found", track_id.as_str()))?;
            let planned = manager.plan_track_update(track_id, |candidate_manager| {
                candidate_manager
                    .configuration_activation_transition(
                        track_id,
                        pending.previous_definition.clone(),
                    )
                    .map_err(anyhow::Error::new)
            })?;
            let (transition, _) = planned.into_parts();
            (previous_state, transition?)
        };
        self.commit_planned_track_mutation(
            guard,
            track_id.as_str(),
            &previous_state,
            transition.next_state(),
            &transition,
            None,
            false,
            None,
        )
        .await
        .map_err(anyhow::Error::new)?;
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: track_id.clone(),
        });
        Ok(())
    }

    async fn reclose_configuration_application_gate(&self, track_id: &TrackId) -> Result<()> {
        let guard = self.lock_track_mutation(track_id.as_str()).await;
        let (previous_state, transition) = {
            let manager = self.manager.read().await;
            let previous_state = manager
                .get_track(track_id.as_str())
                .cloned()
                .ok_or_else(|| anyhow!("track `{}` not found", track_id.as_str()))?;
            let planned = manager.plan_track_update(track_id, |manager| {
                manager.configuration_application_wait_transition(track_id)
            })?;
            let (transition, _) = planned.into_parts();
            (previous_state, transition?)
        };
        self.commit_planned_track_mutation(
            guard,
            track_id.as_str(),
            &previous_state,
            transition.next_state(),
            &transition,
            None,
            false,
            None,
        )
        .await
        .map_err(anyhow::Error::new)?;
        let superseded_submit_effect_ids = self
            .session_effect_queue
            .supersede_queued_submit_effects_for_track(track_id);
        self.record_effects_superseded(track_id.as_str(), &superseded_submit_effect_ids)
            .await?;
        Ok(())
    }

    pub fn effect_journal_diagnostics(&self) -> Arc<crate::EffectJournalDiagnostics> {
        Arc::clone(&self.effect_journal_diagnostics)
    }

    pub async fn handle(
        &self,
        track_id: &TrackId,
        input: TrackInput,
    ) -> Result<TrackUpdateReceipt> {
        let id = track_id.as_str();
        match input {
            TrackInput::Command(command) => {
                self.command(id, command.clone()).await?;
                let kind = match command {
                    poise_engine::command::TrackCommand::Pause => {
                        Some(crate::TrackActivityKind::CommandPause)
                    }
                    poise_engine::command::TrackCommand::Resume => {
                        Some(crate::TrackActivityKind::CommandResume)
                    }
                    poise_engine::command::TrackCommand::Terminate => {
                        Some(crate::TrackActivityKind::CommandTerminate)
                    }
                    poise_engine::command::TrackCommand::Flatten => {
                        Some(crate::TrackActivityKind::CommandFlatten)
                    }
                    poise_engine::command::TrackCommand::Reconcile => None,
                };
                if let Some(kind) = kind
                    && let Err(error) = self
                        .mutation_store
                        .record_track_activity(&crate::StoredTrackActivity {
                            id: 0,
                            track_id: track_id.clone(),
                            kind,
                            reference_id: None,
                            change_fields: Vec::new(),
                            created_at: chrono::Utc::now(),
                        })
                        .await
                {
                    tracing::warn!(track_id = %track_id.as_str(), ?error, "failed to record accepted track command activity");
                }
                self.committed_receipt(id).await
            }
            TrackInput::Market {
                observation,
                occurred_at,
            } => {
                self.observe_market_at(id, observation, occurred_at).await?;
                self.committed_receipt(id).await
            }
            TrackInput::RefreshMarketDataHealth => {
                self.refresh_market_data_health(id).await?;
                self.committed_receipt(id).await
            }
            TrackInput::Position(observation) => {
                self.observe_position(id, observation).await?;
                self.committed_receipt(id).await
            }
            TrackInput::Order(observation) => {
                let (_, absorb_result) = self
                    .observe_order_with_absorb_result(id, observation)
                    .await?;
                Ok(TrackUpdateReceipt::OrderObserved { absorb_result })
            }
            TrackInput::TrackPnl(record) => {
                let inserted = self.record_track_pnl(id, record).await?;
                Ok(TrackUpdateReceipt::PnlRecorded { inserted })
            }
            TrackInput::DurableEvents(events) => {
                self.record_track_events(id, &events).await?;
                self.committed_receipt(id).await
            }
            TrackInput::PrepareFreshSession => {
                self.prepare_fresh_session_for_activation(id).await?;
                self.committed_receipt(id).await
            }
            TrackInput::StartFreshSession {
                control_state,
                pnl_stats,
                external_inputs,
            } => {
                let track_found = self
                    .fresh_start_track_runtime(track_id, control_state, pnl_stats, external_inputs)
                    .await?;
                Ok(TrackUpdateReceipt::FreshSessionStarted { track_found })
            }
            TrackInput::SyncExchangeState {
                token,
                position,
                open_orders,
                reconcile,
            } => {
                anyhow::ensure!(
                    token.track_id == *track_id,
                    "exchange state token belongs to track `{}`, not `{}`",
                    token.track_id.as_str(),
                    id
                );
                let outcome = if reconcile {
                    self.sync_exchange_state(token, position, open_orders)
                        .await?
                } else {
                    self.sync_exchange_state_without_reconcile(token, position, open_orders)
                        .await?
                };
                if matches!(outcome, ExchangeStateSyncOutcome::Applied) {
                    self.advance_configuration_application_after_snapshot(track_id)
                        .await?;
                }
                Ok(TrackUpdateReceipt::ExchangeStateSynced(outcome))
            }
            TrackInput::EffectsSuperseded { effect_ids } => {
                self.record_effects_superseded(id, &effect_ids).await?;
                self.committed_receipt(id).await
            }
            TrackInput::SubmitOutcomeUnknown {
                effect_id,
                client_order_id,
                initial_snapshot_confirmed,
            } => {
                let changed = self
                    .record_submit_outcome_unknown(
                        id,
                        &effect_id,
                        &client_order_id,
                        initial_snapshot_confirmed,
                    )
                    .await?;
                Ok(TrackUpdateReceipt::SubmitOutcomeUnknownRecorded { changed })
            }
            TrackInput::SubmitCompleted {
                effect_id,
                request,
                desired_exposure,
                receipt,
            } => {
                self.complete_submit_execution(
                    id,
                    &effect_id,
                    &request,
                    desired_exposure,
                    &receipt,
                )
                .await?;
                Ok(TrackUpdateReceipt::SubmitAttemptFinished)
            }
            TrackInput::SubmitFailed {
                effect_id,
                client_order_id,
                error,
            } => {
                self.record_submit_failure(id, &effect_id, &client_order_id, &error)
                    .await?;
                Ok(TrackUpdateReceipt::SubmitAttemptFinished)
            }
            TrackInput::SubmitEffectFailed { effect_id, error } => {
                self.complete_submit_effect_failed(id, &effect_id, &error)
                    .await?;
                Ok(TrackUpdateReceipt::SubmitAttemptFinished)
            }
            TrackInput::RecoverSubmit {
                effect_id,
                recovery_token,
                order_lookup,
            } => Ok(TrackUpdateReceipt::SubmitRecovery(
                self.recover_submit_execution(id, &effect_id, &recovery_token, &order_lookup)
                    .await?,
            )),
            TrackInput::CancelOrderSucceeded {
                effect_id,
                order_id,
                receipt,
            } => Ok(TrackUpdateReceipt::CancelOrderResolved(
                self.record_cancel_order_success(id, &effect_id, &order_id, &receipt)
                    .await?,
            )),
            TrackInput::CancelAllSucceeded { effect_id } => {
                self.record_cancel_all_success(id, &effect_id).await?;
                self.committed_receipt(id).await
            }
            TrackInput::EffectSucceeded { effect_id } => {
                self.complete_effect_succeeded(id, &effect_id).await?;
                self.committed_receipt(id).await
            }
            TrackInput::EffectFailed { effect_id, error } => {
                self.complete_effect_failed(id, &effect_id, &error).await?;
                self.committed_receipt(id).await
            }
        }
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub async fn handle_market_for_test(
        &self,
        id: &str,
        observation: MarketObservation,
    ) -> Result<TrackUpdateReceipt> {
        let occurred_at = self.manager.read().await.clock().now();
        self.handle(
            &TrackId::new(id),
            TrackInput::Market {
                observation,
                occurred_at,
            },
        )
        .await
    }

    async fn committed_receipt(&self, id: &str) -> Result<TrackUpdateReceipt> {
        let manager = self.manager.read().await;
        let state = manager
            .get_track(id)
            .ok_or_else(|| anyhow!("track `{id}` not found"))?;
        Ok(TrackUpdateReceipt::Committed {
            decision_revision: state.inventory_decision_revision(),
        })
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> SharedManager {
        Arc::clone(&self.manager)
    }

    async fn prepare_fresh_session_for_activation(&self, id: &str) -> Result<()> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let track_id = TrackId::new(id);
        self.session_effect_queue.clear_track(&track_id);

        let mut manager = self.manager.write().await;
        manager
            .reset_executor_for_activation(&track_id)
            .map_err(TrackMutationError::Mutation)?;
        Ok(())
    }

    async fn fresh_start_track_runtime(
        &self,
        track_id: &TrackId,
        control_state: TrackControlState,
        pnl_stats: TrackPnlStats,
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
            pnl_stats,
            external_inputs,
        )?;
        Ok(true)
    }

    pub(crate) fn emit_internal_notification(&self, notification: ApplicationNotification) {
        let _ = self.notifications.send(notification);
    }

    fn record_effect_journal_failure(
        &self,
        id: &str,
        operation: &'static str,
        error: impl ToString,
    ) {
        self.effect_journal_diagnostics
            .record_failure(&TrackId::new(id), operation, error);
    }

    pub async fn has_track(&self, id: &str) -> bool {
        let manager = self.manager.read().await;
        manager.get_track(id).is_some()
    }

    pub fn has_unresolved_submit_outcome(&self, track_id: &TrackId) -> bool {
        self.session_effect_queue
            .has_unresolved_submit_outcome_for_track(track_id)
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

    /// A single committed manager snapshot for read-only configuration versioning.
    pub async fn committed_track_definitions(&self) -> Vec<poise_core::track::TrackDefinition> {
        let manager = self.manager.read().await;
        let mut definitions = manager
            .list_tracks()
            .into_iter()
            .map(|track| track.definition().clone())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.track_id().as_str().cmp(right.track_id().as_str()));
        definitions
    }

    pub(crate) async fn committed_track_snapshots(&self) -> Result<Vec<CommittedTrackSnapshot>> {
        let manager = self.manager.read().await;
        manager
            .list_tracks()
            .into_iter()
            .map(|track| {
                Ok(CommittedTrackSnapshot {
                    definition: track.definition().clone(),
                    runtime: manager.track_runtime_view(track.id())?,
                })
            })
            .collect()
    }

    pub(crate) async fn committed_track_snapshot(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<CommittedTrackSnapshot>> {
        let manager = self.manager.read().await;
        let Some(track) = manager.get_track(track_id.as_str()) else {
            return Ok(None);
        };
        Ok(Some(CommittedTrackSnapshot {
            definition: track.definition().clone(),
            runtime: manager.track_runtime_view(track_id)?,
        }))
    }

    pub async fn resolve_track_id(&self, instrument: &Instrument) -> Option<String> {
        let manager = self.manager.read().await;
        manager
            .resolve_track_id(instrument)
            .map(|track_id| track_id.as_str().to_string())
    }
}

impl TradingRuntime {
    async fn observe_market_at(
        &self,
        id: &str,
        observation: MarketObservation,
        occurred_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<TrackTransition> {
        let mutation_guard = self.lock_track_mutation(id).await;
        let (previous_state, outcome, next_state) = self
            .plan_manager_mutation(id, |manager| {
                manager.observe_market_mutation(&TrackId::new(id), observation.clone(), occurred_at)
            })
            .await
            .map_err(anyhow::Error::new)?;

        match outcome.commit_intent() {
            CommitIntent::LiveOnly => {
                self.manager
                    .write()
                    .await
                    .install_planned_runtime(next_state.clone());
                self.notify_live_only_executor_changes(id, &previous_state, &next_state);
                self.session_effect_queue
                    .wake_track_for(&TrackId::new(id), WakeSignal::FreshMarket);

                Ok(outcome)
            }
            CommitIntent::Commit => {
                let transition = outcome;
                let _mutation_guard = self
                    .commit_planned_track_mutation(
                        mutation_guard,
                        id,
                        &previous_state,
                        transition.next_state(),
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

    #[cfg(test)]
    async fn observe_market(
        &self,
        id: &str,
        observation: MarketObservation,
    ) -> Result<TrackTransition> {
        let occurred_at = self.manager.read().await.clock().now();
        self.observe_market_at(id, observation, occurred_at).await
    }

    fn notify_live_only_executor_changes(
        &self,
        id: &str,
        previous_state: &TrackRuntime,
        next_state: &TrackRuntime,
    ) {
        let recovery_anomaly_changed =
            previous_state.recovery_anomaly_active() != next_state.recovery_anomaly_active();
        if recovery_anomaly_changed {
            self.recovery_anomaly_observer
                .observe_recovery_anomaly_change(
                    &TrackId::new(id),
                    next_state.recovery_anomaly_active(),
                );
        }
        if previous_state.executor_view_changed(next_state) {
            self.emit_internal_notification(ApplicationNotification::TrackChanged {
                track_id: TrackId::new(id),
            });
        }
    }

    async fn command(&self, id: &str, command: TrackCommand) -> Result<TrackTransition> {
        let mutation_guard = self.lock_track_mutation(id).await;
        let (previous_state, transition, next_state) = self
            .plan_manager_mutation(id, |candidate_manager| {
                candidate_manager.command(&TrackId::new(id), command.clone())
            })
            .await
            .map_err(anyhow::Error::new)?;
        let control_state_override =
            persisted_control_state_after_command(previous_state.runtime_state(), &command);

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
                &transition,
                None,
                false,
                control_state_override.as_ref(),
            )
            .await
            .map_err(anyhow::Error::new)?;
        Ok(transition)
    }

    async fn refresh_market_data_health(&self, id: &str) -> Result<TrackTransition> {
        self.mutate_track_skip_when_no_work(id, |manager| {
            manager.refresh_market_data_health(&TrackId::new(id))
        })
        .await
        .map_err(anyhow::Error::new)
    }

    pub async fn market_data_health_deadline(
        &self,
        id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let manager = self.manager.read().await;
        manager.market_data_health_deadline(&TrackId::new(id))
    }

    pub async fn track_live_view(&self, id: &str) -> Result<TrackLiveView> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let manager = self.manager.read().await;
        manager.track_live_view(&TrackId::new(id))
    }

    pub async fn track_runtime_view(&self, id: &str) -> Result<Option<TrackRuntimeView>> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let manager = self.manager.read().await;
        if manager.get_track(id).is_none() {
            return Ok(None);
        }
        manager.track_runtime_view(&TrackId::new(id)).map(Some)
    }

    pub async fn quote_health_view(&self, id: &str) -> Result<QuoteHealthView> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let manager = self.manager.read().await;
        manager.quote_health_view(&TrackId::new(id))
    }

    pub async fn strategy_target_view(&self, id: &str) -> Result<StrategyTargetView> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let manager = self.manager.read().await;
        manager.strategy_target_view(&TrackId::new(id))
    }

    async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<TrackTransition> {
        self.mutate_track(id, |manager| {
            manager.observe_position_update(&TrackId::new(id), observation.clone())
        })
        .await
        .map_err(anyhow::Error::new)
    }

    async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, OrderUpdateAbsorbResult)> {
        let mutation_guard = self.lock_track_mutation(id).await;
        let client_order_id = observation.client_order_id.clone();
        let (previous_state, result, next_state) = self
            .plan_manager_mutation(id, |manager| {
                manager.observe_order_update(&TrackId::new(id), observation)
            })
            .await
            .map_err(anyhow::Error::new)?;

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
                &result,
                None,
                false,
                None,
            )
            .await
            .map_err(anyhow::Error::new)?;

        if result.1 != OrderUpdateAbsorbResult::Unabsorbed && !next_state.recovery_anomaly_active()
        {
            let resolved_effect_ids = self
                .session_effect_queue
                .resolve_submit_awaiting_resolution_by_client_order_id(
                    &TrackId::new(id),
                    &client_order_id,
                );
            self.record_effects_succeeded(id, &resolved_effect_ids)
                .await?;
        }

        Ok(result)
    }

    async fn record_submit_outcome_unknown(
        &self,
        id: &str,
        effect_id: &str,
        client_order_id: &str,
        initial_snapshot_confirmed: bool,
    ) -> Result<bool> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let state_facts = {
            let manager = self.manager.read().await;
            manager
                .get_track(id)
                .map(|state| {
                    (
                        state.recovery_anomaly_active(),
                        state.has_recorded_order_fact_for_client_order_id(client_order_id),
                    )
                })
                .ok_or_else(|| anyhow!("track `{id}` not found"))
        };
        let effective_initial_snapshot_confirmed = initial_snapshot_confirmed
            && state_facts
                .as_ref()
                .is_ok_and(|(recovery_anomaly_active, _)| !recovery_anomaly_active);
        if !self
            .session_effect_queue
            .record_submit_outcome_unknown(effect_id, effective_initial_snapshot_confirmed)
        {
            return Ok(false);
        }

        let (recovery_anomaly_active, recorded_order_fact) = state_facts?;
        if recovery_anomaly_active || !recorded_order_fact {
            return Ok(true);
        }

        let resolved_effect_ids = self
            .session_effect_queue
            .resolve_submit_awaiting_resolution_by_client_order_id(
                &TrackId::new(id),
                client_order_id,
            );
        self.record_effects_succeeded(id, &resolved_effect_ids)
            .await?;
        Ok(true)
    }

    async fn record_track_pnl(&self, id: &str, record: TrackPnlRecord) -> Result<bool> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let track_id = TrackId::new(id);
        let (_, (), next_state) = self
            .plan_manager_mutation(id, |candidate| {
                candidate.apply_pnl_record(&track_id, &record)
            })
            .await
            .map_err(anyhow::Error::new)?;

        let inserted = self
            .mutation_store
            .insert_track_pnl_record(&track_id, &record)
            .await?;
        if inserted {
            {
                let mut manager = self.manager.write().await;
                manager.install_planned_runtime(next_state);
            }
            self.emit_internal_notification(ApplicationNotification::TrackChanged { track_id });
        }
        Ok(inserted)
    }

    async fn record_track_events(&self, id: &str, events: &[DomainEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let _mutation_guard = self.lock_track_mutation(id).await;
        {
            let manager = self.manager.read().await;
            if manager.get_track(id).is_none() {
                return Err(anyhow!("track `{id}` not found"));
            }
        }

        self.mutation_store
            .commit_track_transition(id, None, events)
            .await?;
        self.emit_internal_notification(ApplicationNotification::TrackChanged {
            track_id: TrackId::new(id),
        });
        Ok(())
    }

    pub async fn prepare_exchange_state_sync(&self, id: &str) -> Result<ExchangeStateSyncToken> {
        let _mutation_guard = self.lock_track_mutation(id).await;
        let track_id = TrackId::new(id);
        let observation_revision = self
            .manager
            .read()
            .await
            .exchange_observation_revision(&track_id)?;
        let queue_revision = self.session_effect_queue.exchange_state_revision(&track_id);
        Ok(ExchangeStateSyncToken {
            track_id,
            observation_revision,
            queue_revision,
        })
    }

    async fn sync_exchange_state(
        &self,
        token: ExchangeStateSyncToken,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
    ) -> Result<ExchangeStateSyncOutcome> {
        self.sync_exchange_state_inner(
            token,
            position,
            open_orders,
            ExchangeSyncMode::RecoverAndReconcile,
        )
        .await
    }

    async fn sync_exchange_state_without_reconcile(
        &self,
        token: ExchangeStateSyncToken,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
    ) -> Result<ExchangeStateSyncOutcome> {
        self.sync_exchange_state_inner(token, position, open_orders, ExchangeSyncMode::RecoverOnly)
            .await
    }

    async fn sync_exchange_state_inner(
        &self,
        token: ExchangeStateSyncToken,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
        mode: ExchangeSyncMode,
    ) -> Result<ExchangeStateSyncOutcome> {
        let id = token.track_id.as_str();
        let mutation_guard = self.lock_track_mutation(id).await;
        let (
            previous_state,
            transition,
            next_state,
            follow_up_plan,
            submit_awaiting_initial_snapshot_effect_ids,
            submit_awaiting_resolution_effect_ids,
            exchange_state_wake_token,
        ) = {
            let (previous_state, planned_update) = {
                let manager = self.manager.read().await;
                if manager.exchange_observation_revision(&token.track_id)?
                    != token.observation_revision
                {
                    return Ok(ExchangeStateSyncOutcome::RevisionChanged);
                }
                let previous_state = manager
                    .get_track(id)
                    .cloned()
                    .ok_or_else(|| anyhow!("track `{id}` not found"))?;
                let planned_update =
                    manager.plan_track_update(&token.track_id, |candidate_manager| {
                        self.sync_account_capacity_gate_state(candidate_manager, id)?;
                        Ok(self
                            .session_effect_queue
                            .with_exchange_snapshot_context_if_current(
                                &token.track_id,
                                token.queue_revision,
                                &open_orders,
                                |queue_context| {
                                    let follow_up_plan = queue_context.follow_up_plan;
                                    let effective_mode = if follow_up_plan.requires_reconcile() {
                                        ExchangeSyncMode::RecoverAndReconcile
                                    } else {
                                        mode
                                    };
                                    let transition = if effective_mode.allows_follow_up_reconcile()
                                    {
                                        candidate_manager
                                            .sync_exchange_state(
                                                &token.track_id,
                                                position,
                                                open_orders.clone(),
                                                queue_context.pending_submit_hints,
                                            )
                                            .map_err(TrackMutationError::Mutation)?
                                    } else {
                                        candidate_manager
                                            .sync_exchange_state_without_reconcile(
                                                &token.track_id,
                                                position,
                                                open_orders.clone(),
                                                queue_context.pending_submit_hints,
                                            )
                                            .map_err(TrackMutationError::Mutation)?
                                    };
                                    Ok::<_, TrackMutationError>((
                                        transition,
                                        follow_up_plan,
                                        queue_context.submit_awaiting_initial_snapshot_effect_ids,
                                        queue_context.submit_awaiting_resolution_effect_ids,
                                        queue_context.exchange_state_wake_token,
                                    ))
                                },
                            ))
                    })?;
                (previous_state, planned_update)
            };
            let (planned_result, next_state) = planned_update.into_parts();
            let Some(result) = planned_result? else {
                return Ok(ExchangeStateSyncOutcome::RevisionChanged);
            };
            let candidate_revision = next_state.exchange_observation_revision();
            self.manager
                .write()
                .await
                .retain_exchange_observation_revision(&token.track_id, candidate_revision)?;
            let (
                transition,
                follow_up_plan,
                submit_awaiting_initial_snapshot_effect_ids,
                submit_awaiting_resolution_effect_ids,
                exchange_state_wake_token,
            ) = result.map_err(anyhow::Error::new)?;
            (
                previous_state,
                transition,
                next_state,
                follow_up_plan,
                submit_awaiting_initial_snapshot_effect_ids,
                submit_awaiting_resolution_effect_ids,
                exchange_state_wake_token,
            )
        };

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
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
        self.handle_follow_up_queue_actions(id, &follow_up_actions)
            .await?;

        let mut resolved_initial_snapshot_effect_ids = Vec::new();
        if !next_state.recovery_anomaly_active() {
            resolved_initial_snapshot_effect_ids = self
                .session_effect_queue
                .resolve_submit_initial_snapshot_effects_with_recorded_order_fact(
                    &token.track_id,
                    &submit_awaiting_initial_snapshot_effect_ids,
                    |client_order_id| {
                        next_state.has_recorded_order_fact_for_client_order_id(client_order_id)
                    },
                );
            self.session_effect_queue
                .promote_submit_unknown_after_initial_snapshot(
                    &token.track_id,
                    &submit_awaiting_initial_snapshot_effect_ids,
                );
        }

        let mut resolved_submit_effect_ids = if next_state.recovery_anomaly_active() {
            Vec::new()
        } else {
            self.session_effect_queue
                .resolve_submit_awaiting_resolution_effects(
                    &token.track_id,
                    &submit_awaiting_resolution_effect_ids,
                )
        };
        resolved_submit_effect_ids.append(&mut resolved_initial_snapshot_effect_ids);
        self.record_effects_succeeded(id, &resolved_submit_effect_ids)
            .await?;

        if let Some(wake_token) = exchange_state_wake_token.as_ref() {
            self.session_effect_queue
                .wake_track_for_if_current_deferral(
                    &token.track_id,
                    wake_token,
                    WakeSignal::ExchangeState,
                );
        }

        Ok(ExchangeStateSyncOutcome::Applied)
    }

    async fn handle_follow_up_queue_actions(
        &self,
        id: &str,
        actions: &[FollowUpQueueAction],
    ) -> std::result::Result<(), TrackMutationError> {
        let mut journal_outcomes = Vec::new();

        for action in actions {
            match action {
                FollowUpQueueAction::Closed {
                    cancel_effect_id,
                    superseded_downstream_effect_ids,
                    requires_reconcile: _,
                } => {
                    journal_outcomes.push(EffectStatusUpdate::succeeded(cancel_effect_id.clone()));
                    journal_outcomes.extend(
                        superseded_downstream_effect_ids
                            .iter()
                            .cloned()
                            .map(EffectStatusUpdate::superseded),
                    );
                }
                FollowUpQueueAction::StillOpen { .. } => {}
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
            self.record_effect_journal_failure(id, "record_cancel_follow_up_outcomes", &error);
            tracing::warn!(
                track_id = id,
                "failed to record cancel follow-up journal outcomes: {error}"
            );
        }

        Ok(())
    }

    async fn record_effects_superseded(&self, id: &str, effect_ids: &[String]) -> Result<()> {
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
            self.record_effect_journal_failure(id, "record_outcomes", &error);
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

    async fn complete_submit_execution(
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
                )
            },
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    async fn record_submit_failure(
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

    async fn complete_submit_effect_failed(
        &self,
        id: &str,
        effect_id: &str,
        error: &str,
    ) -> Result<()> {
        self.complete_effect_failed(id, effect_id, error).await?;
        Ok(())
    }

    async fn record_cancel_order_success(
        &self,
        id: &str,
        effect_id: &str,
        order_id: &str,
        receipt: &OrderReceipt,
    ) -> Result<CancelReceiptResolution> {
        let mutation_guard = self.lock_track_mutation(id).await;
        let cancel_effect_status_update = EffectStatusUpdate::succeeded(effect_id.to_string());
        let (previous_state, (), next_state) = self
            .plan_manager_mutation(id, |manager| {
                manager.record_cancel_order_success(&TrackId::new(id), order_id, receipt)
            })
            .await
            .map_err(anyhow::Error::new)?;
        let cancel_progressed =
            next_state.has_absorbed_binding_for_cancel_receipt(order_id, receipt);

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
                &(),
                Some(&cancel_effect_status_update),
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

    async fn record_cancel_all_success(&self, id: &str, effect_id: &str) -> Result<()> {
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

    async fn complete_effect_succeeded(&self, id: &str, effect_id: &str) -> Result<()> {
        self.mutate_track_with_effect_status(
            id,
            EffectStatusUpdate::succeeded(effect_id.to_string()),
            |_manager| Ok(()),
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    async fn complete_effect_failed(&self, id: &str, effect_id: &str, error: &str) -> Result<()> {
        self.mutate_track_with_effect_status(
            id,
            effect_status_failed(effect_id, error),
            |_manager| Ok(()),
        )
        .await
        .map(|_| ())
        .map_err(anyhow::Error::new)
    }

    async fn recover_submit_execution(
        &self,
        id: &str,
        effect_id: &str,
        recovery_token: &SubmitRecoveryToken,
        order_lookup: &SubmitRecoveryOrderLookup,
    ) -> Result<SubmitRecoveryReceipt> {
        Ok(
            match self
                .recover_submit_effect(id, effect_id, recovery_token, order_lookup)
                .await?
            {
                SubmitRecoveryResolution::Proceed {
                    request,
                    desired_exposure,
                    ..
                } => SubmitRecoveryReceipt::Dispatch {
                    request,
                    desired_exposure,
                },
                SubmitRecoveryResolution::Recovered { .. }
                | SubmitRecoveryResolution::Superseded { .. }
                | SubmitRecoveryResolution::AwaitExchangeState => SubmitRecoveryReceipt::Finished,
            },
        )
    }

    async fn recover_submit_effect(
        &self,
        id: &str,
        effect_id: &str,
        recovery_token: &SubmitRecoveryToken,
        order_lookup: &SubmitRecoveryOrderLookup,
    ) -> Result<SubmitRecoveryResolution> {
        let mutation_guard = self.lock_track_mutation(id).await;
        let (previous_state, plan, next_state) = self
            .plan_manager_mutation(id, |manager| {
                manager.recover_submit_effect(&TrackId::new(id), recovery_token, order_lookup)
            })
            .await
            .map_err(anyhow::Error::new)?;

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

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
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
        let mutation_guard = self.lock_track_mutation(id).await;
        let (previous_state, result, next_state) = self.plan_manager_mutation(id, mutate).await?;

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
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

    async fn mutate_track_skip_when_no_work<R, F>(
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
        skip_when_no_work: bool,
        mutate: F,
    ) -> std::result::Result<R, TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
        R: TransitionResult,
    {
        let mutation_guard = self.lock_track_mutation(id).await;
        let (previous_state, result, next_state) = self.plan_manager_mutation(id, mutate).await?;

        let _mutation_guard = self
            .commit_planned_track_mutation(
                mutation_guard,
                id,
                &previous_state,
                &next_state,
                &result,
                None,
                skip_when_no_work,
                None,
            )
            .await?;
        Ok(result)
    }

    async fn plan_manager_mutation<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> std::result::Result<(TrackRuntime, R, TrackRuntime), TrackMutationError>
    where
        F: FnOnce(&mut TrackManager) -> Result<R>,
    {
        let (previous_state, planned_update) = {
            let manager = self.manager.read().await;
            let previous_state = manager
                .get_track(id)
                .cloned()
                .ok_or_else(|| TrackMutationError::loaded_track_invariant(id))?;
            let planned_update = manager
                .plan_track_update(&TrackId::new(id), |candidate_manager| {
                    self.sync_account_capacity_gate_state(candidate_manager, id)?;
                    mutate(candidate_manager)
                })
                .map_err(TrackMutationError::Mutation)?;
            (previous_state, planned_update)
        };
        let track_id = TrackId::new(id);
        let (result, next_state) = planned_update.into_parts();
        let candidate_revision = next_state.exchange_observation_revision();
        self.manager
            .write()
            .await
            .retain_exchange_observation_revision(&track_id, candidate_revision)
            .map_err(TrackMutationError::Mutation)?;
        let result = result.map_err(TrackMutationError::Mutation)?;
        Ok((previous_state, result, next_state))
    }

    async fn lock_track_mutation(&self, id: &str) -> OwnedMutexGuard<()> {
        self.mutation_guards.lock(id).await
    }

    fn sync_account_capacity_gate_state(&self, manager: &mut TrackManager, id: &str) -> Result<()> {
        let Some(track) = manager.get_track(id) else {
            return Ok(());
        };
        let available_notional = self
            .account_margin_guard
            .available_notional_for(track.instrument());
        manager.set_account_capacity_available_notional(id, available_notional)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn commit_track_mutation<R>(
        &self,
        mutation_guard: OwnedMutexGuard<()>,
        id: &str,
        previous_state: &TrackRuntime,
        next_state: &TrackRuntime,
        result: &R,
        effect_status_update: Option<&EffectStatusUpdate>,
        skip_when_no_work: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<OwnedMutexGuard<()>, TrackMutationError>
    where
        R: TransitionResult,
    {
        let effect_status_updates = match effect_status_update {
            Some(update) => std::slice::from_ref(update),
            None => &[],
        };
        self.commit_track_mutation_with_effect_status_updates(
            mutation_guard,
            id,
            previous_state,
            next_state,
            result,
            effect_status_updates,
            skip_when_no_work,
            control_state_override,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_planned_track_mutation<R>(
        &self,
        mutation_guard: OwnedMutexGuard<()>,
        id: &str,
        previous_state: &TrackRuntime,
        next_state: &TrackRuntime,
        result: &R,
        effect_status_update: Option<&EffectStatusUpdate>,
        skip_when_no_work: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<OwnedMutexGuard<()>, TrackMutationError>
    where
        R: TransitionResult,
    {
        let effect_status_updates = match effect_status_update {
            Some(update) => std::slice::from_ref(update),
            None => &[],
        };
        self.commit_track_mutation_with_effect_status_updates_and_install(
            mutation_guard,
            id,
            previous_state,
            next_state,
            result,
            effect_status_updates,
            skip_when_no_work,
            control_state_override,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn commit_track_mutation_with_effect_status_updates<R>(
        &self,
        mutation_guard: OwnedMutexGuard<()>,
        id: &str,
        previous_state: &TrackRuntime,
        next_state: &TrackRuntime,
        result: &R,
        effect_status_updates: &[EffectStatusUpdate],
        skip_when_no_work: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<OwnedMutexGuard<()>, TrackMutationError>
    where
        R: TransitionResult,
    {
        self.commit_track_mutation_with_effect_status_updates_and_install(
            mutation_guard,
            id,
            previous_state,
            next_state,
            result,
            effect_status_updates,
            skip_when_no_work,
            control_state_override,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_track_mutation_with_effect_status_updates_and_install<R>(
        &self,
        mutation_guard: OwnedMutexGuard<()>,
        id: &str,
        previous_state: &TrackRuntime,
        next_state: &TrackRuntime,
        result: &R,
        effect_status_updates: &[EffectStatusUpdate],
        skip_when_no_work: bool,
        control_state_override: Option<&TrackControlState>,
    ) -> std::result::Result<OwnedMutexGuard<()>, TrackMutationError>
    where
        R: TransitionResult,
    {
        let commit = OwnedTrackMutationCommit {
            id: id.to_string(),
            previous_state: previous_state.clone(),
            next_state: next_state.clone(),
            domain_events: result.domain_events().to_vec(),
            effects: result.effects().to_vec(),
            effect_status_updates: effect_status_updates.to_vec(),
            skip_when_no_work,
            control_state_override: control_state_override.cloned(),
        };
        let executor = self.clone();
        // A request future may be cancelled while persistence is pending. The spawned owner keeps
        // the per-track guard until the shared commit path finishes.
        let task = tokio::spawn(async move {
            let result = executor.commit_owned_track_mutation(commit).await;
            (mutation_guard, result)
        });
        let (mutation_guard, result) = task.await.map_err(|error| {
            TrackMutationError::Persistence(anyhow!("track mutation commit task failed: {error}"))
        })?;
        result?;
        Ok(mutation_guard)
    }

    async fn commit_owned_track_mutation(
        &self,
        commit: OwnedTrackMutationCommit,
    ) -> std::result::Result<(), TrackMutationError> {
        let OwnedTrackMutationCommit {
            id,
            previous_state,
            next_state,
            domain_events,
            effects,
            effect_status_updates,
            skip_when_no_work,
            control_state_override,
        } = commit;
        let control_state_update = persisted_control_state_after_transition(
            previous_state.runtime_state(),
            next_state.runtime_state(),
            control_state_override.as_ref(),
        );
        let has_session_effects = !effects.is_empty();
        let has_track_write = control_state_update.is_some() || !domain_events.is_empty();
        let has_effect_status_update = !effect_status_updates.is_empty();
        let has_work = has_track_write || has_effect_status_update || has_session_effects;
        let decision_changed = previous_state.inventory_decision_revision()
            != next_state.inventory_decision_revision();
        let market_data_stale_changed =
            previous_state.market_data_stale() != next_state.market_data_stale();
        let executor_view_changed = previous_state.executor_view_changed(&next_state);
        let public_view_changed =
            decision_changed || market_data_stale_changed || executor_view_changed;
        let previous_recovery_anomaly_active = previous_state.recovery_anomaly_active();
        let next_recovery_anomaly_active = next_state.recovery_anomaly_active();
        let recovery_anomaly_changed =
            previous_recovery_anomaly_active != next_recovery_anomaly_active;
        if skip_when_no_work && !has_work && !public_view_changed && !recovery_anomaly_changed {
            return Ok(());
        }

        if has_track_write {
            let persistence_result = self
                .mutation_store
                .commit_track_transition(&id, control_state_update.as_ref(), &domain_events)
                .await;

            persistence_result.map_err(TrackMutationError::Persistence)?;
        }

        self.manager
            .write()
            .await
            .install_planned_runtime(next_state);

        let effect_created_at = chrono::Utc::now();
        let enqueued_effects = self.session_effect_queue.enqueue_transition_effects(
            &TrackId::new(id.as_str()),
            &effects,
            effect_created_at,
        );
        if !enqueued_effects.is_empty() {
            let journal_entries =
                effect_journal_entries_from_enqueued(enqueued_effects.journal_projection_entries());
            if let Err(error) = self.effect_journal.append_entries(&journal_entries).await {
                self.record_effect_journal_failure(id.as_str(), "append_entries", &error);
                tracing::warn!(
                    track_id = id,
                    "failed to append effect journal entries: {error}"
                );
            }
        }

        if has_effect_status_update
            && let Err(error) = self
                .effect_journal
                .record_effect_outcomes(&effect_status_updates)
                .await
        {
            self.record_effect_journal_failure(id.as_str(), "record_outcomes", &error);
            tracing::warn!(
                track_id = id,
                "failed to record effect journal outcomes: {error}"
            );
        }

        if recovery_anomaly_changed {
            self.recovery_anomaly_observer
                .observe_recovery_anomaly_change(
                    &TrackId::new(id.as_str()),
                    next_recovery_anomaly_active,
                );
        }

        if has_work || public_view_changed || recovery_anomaly_changed {
            self.emit_internal_notification(ApplicationNotification::TrackChanged {
                track_id: TrackId::new(id.as_str()),
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

fn classify_cancel_receipt_resolution(
    order_id: &str,
    cancel_progressed: bool,
    receipt: &OrderReceipt,
) -> CancelReceiptResolution {
    if cancel_progressed || receipt.filled_qty.value() > f64::EPSILON {
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
    use chrono::{NaiveDate, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::instrument::PositionQty;
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::clock::ClockPort;
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::ledger::{TrackPnlRecord, TrackPnlStats};
    use poise_engine::manager::TrackManager;

    use crate::exchange::OrderRequest;
    use tokio::sync::broadcast;

    use crate::{
        ApplicationNotification, CommittedTrackWrite, EffectJournalEntry, EffectStatus,
        EffectStatusUpdate, PersistedTrackEffect, SessionEffectQueue, TrackControlState,
        TrackEffectJournal, TrackMutationStore,
    };

    use super::{AccountCapacityGuard, TradingRuntime};

    pub(crate) struct TestRuntimeSet {
        pub(crate) runtime: Arc<TradingRuntime>,
        pub(crate) submit_effect: crate::submit_effect_service::SubmitEffectService,
        pub(crate) runtime_lifecycle: crate::TrackRuntimeLifecycleService,
        pub(crate) session_effect_queue: SessionEffectQueue,
        pub(crate) effect_journal_diagnostics: Arc<crate::EffectJournalDiagnostics>,
    }

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
        pnl_records: Mutex<HashMap<String, Vec<TrackPnlRecord>>>,
        next_pnl_insert_error: Mutex<Option<String>>,
        events: Mutex<HashMap<String, Vec<DomainEvent>>>,
        effects: Mutex<Vec<PersistedTrackEffect>>,
        activities: Mutex<Vec<crate::StoredTrackActivity>>,
        business_write_calls: Mutex<usize>,
        effect_outcome_write_calls: Mutex<usize>,
        effect_journal_append_error: Mutex<Option<String>>,
        effect_journal_outcome_error: Mutex<Option<String>>,
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

        pub(crate) fn activities(&self) -> Vec<crate::StoredTrackActivity> {
            self.activities.lock().unwrap().clone()
        }

        pub(crate) fn fail_effect_journal_appends(&self, error: impl Into<String>) {
            *self.effect_journal_append_error.lock().unwrap() = Some(error.into());
        }

        pub(crate) fn fail_next_effect_journal_outcome(&self, error: impl Into<String>) {
            *self.effect_journal_outcome_error.lock().unwrap() = Some(error.into());
        }

        pub(crate) fn fail_next_pnl_insert(&self, error: impl Into<String>) {
            *self.next_pnl_insert_error.lock().unwrap() = Some(error.into());
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
                        quantity: PositionQty::new(0.1).unwrap(),
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
                            quantity: PositionQty::new(0.1).unwrap(),
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
        async fn list_recent_track_activity(
            &self,
            track_id: &TrackId,
            limit: usize,
        ) -> Result<Vec<crate::StoredTrackActivity>> {
            Ok(self
                .activities
                .lock()
                .unwrap()
                .iter()
                .filter(|activity| activity.track_id == *track_id)
                .rev()
                .take(limit)
                .cloned()
                .collect())
        }

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

        async fn load_track_pnl_stats(
            &self,
            track_id: &TrackId,
            pnl_utc_day: NaiveDate,
        ) -> Result<TrackPnlStats> {
            let mut stats = TrackPnlStats {
                pnl_utc_day,
                ..TrackPnlStats::default()
            };
            for record in self
                .pnl_records
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default()
            {
                stats.apply_record(&record);
            }
            Ok(stats)
        }

        async fn list_track_pnl_source_keys(
            &self,
            track_id: &TrackId,
            source_keys: &[String],
        ) -> Result<Vec<String>> {
            let records = self
                .pnl_records
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default();
            Ok(source_keys
                .iter()
                .filter(|source_key| {
                    records
                        .iter()
                        .any(|record| record.source_key.as_ref() == Some(*source_key))
                })
                .cloned()
                .collect())
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
        async fn record_track_activity(&self, activity: &crate::StoredTrackActivity) -> Result<()> {
            self.activities.lock().unwrap().push(activity.clone());
            Ok(())
        }

        async fn commit_track_transition(
            &self,
            id: &str,
            control_state: Option<&TrackControlState>,
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

        async fn insert_track_pnl_record(
            &self,
            track_id: &TrackId,
            record: &TrackPnlRecord,
        ) -> Result<bool> {
            if let Some(error) = self.next_pnl_insert_error.lock().unwrap().take() {
                anyhow::bail!(error);
            }
            let mut pnl_records = self.pnl_records.lock().unwrap();
            let records = pnl_records
                .entry(track_id.as_str().to_string())
                .or_default();
            if record.source_key.as_ref().is_some_and(|source_key| {
                records
                    .iter()
                    .any(|existing| existing.source_key.as_ref() == Some(source_key))
            }) {
                return Ok(false);
            }
            records.push(record.clone());
            Ok(true)
        }
    }

    #[async_trait]
    impl TrackEffectJournal for MemoryRepository {
        async fn append_entries(&self, entries: &[EffectJournalEntry]) -> Result<()> {
            if let Some(error) = self.effect_journal_append_error.lock().unwrap().clone() {
                anyhow::bail!(error);
            }
            self.effects
                .lock()
                .unwrap()
                .extend(entries.iter().cloned().map(PersistedTrackEffect::from));
            Ok(())
        }

        async fn record_effect_outcomes(&self, outcomes: &[EffectStatusUpdate]) -> Result<()> {
            if let Some(error) = self.effect_journal_outcome_error.lock().unwrap().take() {
                anyhow::bail!(error);
            }
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
    ) -> (TestRuntimeSet, broadcast::Sender<ApplicationNotification>) {
        let (notifications, _) = broadcast::channel(16);
        let runtime = Arc::new(TradingRuntime::new(
            manager,
            repository.clone(),
            repository.clone(),
            notifications.clone(),
            Arc::new(NoopGuard),
        ));
        let services = TestRuntimeSet {
            runtime: Arc::clone(&runtime),
            submit_effect: crate::submit_effect_service::SubmitEffectService::new(Arc::clone(
                &runtime,
            )),
            runtime_lifecycle: crate::TrackRuntimeLifecycleService::new(
                Arc::clone(&runtime),
                repository,
            ),
            session_effect_queue: runtime.session_effect_queue(),
            effect_journal_diagnostics: runtime.effect_journal_diagnostics(),
        };
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
                        risk_acquisition: Default::default(),
                    },
                    Some(3_000.0),
                    LossLimits {
                        daily_loss_limit: 300.0,
                        total_loss_limit: 600.0,
                    },
                    None,
                )
                .unwrap(),
                crate::test_instrument::linear_spec(0.1, 0.001, 0.001, 5.0),
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
    use std::time::Duration;

    use crate::exchange::{ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus};
    use crate::session_effect_queue::{
        DeferredUntil, SessionEffectOutcome, SessionPendingEffectState, WakeSignal,
    };
    use crate::{
        ApplicationNotification, CancelReceiptResolution, ConfigurationApplicationReceipt,
        EffectStatus, SessionEffectQueue, TrackEffectJournal,
    };
    use async_trait::async_trait;
    use chrono::Utc;
    use poise_core::instrument::{Amount, Asset, PositionQty};
    use poise_core::risk::RiskTerminationCause;
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{Exposure, Side};
    use poise_engine::command::TrackCommand;
    use poise_engine::execution_plan::TrackEffect;
    use poise_engine::executor::{
        BindingStatus, OrderUpdateAbsorbResult, RecoveryAnomaly, SubmitRecoveryToken,
    };
    use poise_engine::ledger::{TrackPnlRecord, TrackPnlStats};
    use poise_engine::manager::SubmitRecoveryOrderLookup;
    use poise_engine::observation::{
        CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
    };
    use poise_engine::runtime::TrackRuntime;
    use poise_engine::runtime::{
        AutoState, ControlState, FreshSessionExternalInputs, InventoryDecisionSnapshot,
        InventoryDecisionView, TerminationCause, TrackState,
    };
    use poise_engine::transition::TrackTransition;
    use tokio::sync::broadcast::error::TryRecvError;
    use tokio::sync::{Semaphore, broadcast};

    use super::test_support::{MemoryRepository, NoopGuard, seeded_manager, track_write_services};
    use super::{
        AccountCapacityGuard, ConfigurationApplicationError, ExchangeStateSyncOutcome,
        RecoveryAnomalyObserver, TrackInput, TrackUpdateReceipt, TradingRuntime,
        effect_journal_entries_from_enqueued,
    };
    use crate::{
        CommittedTrackWrite, ConfigurationApplicationPersistence, ConfigurationApplicationRequest,
        ConfigurationApplicationStatus, ConfigurationApplicationStore,
        ConfigurationApplicationStoreError, TrackControlState, TrackMutationStore, TrackQueryStore,
    };

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    struct FakeConfigurationStore {
        outcome: std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        >,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl ConfigurationApplicationStore for FakeConfigurationStore {
        async fn accept_configuration_application(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            *self.calls.lock().unwrap() += 1;
            self.outcome.clone()
        }

        async fn persist_accepted_configuration(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            *self.calls.lock().unwrap() += 1;
            self.outcome.clone()
        }

        async fn configuration_application_status(
            &self,
            _track_id: &TrackId,
        ) -> std::result::Result<ConfigurationApplicationStatus, ConfigurationApplicationStoreError>
        {
            Ok(ConfigurationApplicationStatus::Idle)
        }

        async fn update_configuration_application_status(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
            _status: ConfigurationApplicationStatus,
        ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
            Ok(())
        }
    }

    struct CompletionRecordingConfigurationStore {
        persist_outcomes: Mutex<Vec<ConfigurationApplicationPersistence>>,
        complete_outcomes: Mutex<Vec<std::result::Result<(), ConfigurationApplicationStoreError>>>,
        completed_statuses: Mutex<Vec<ConfigurationApplicationStatus>>,
        persist_calls: Mutex<usize>,
        assert_queue_empty: Option<SessionEffectQueue>,
    }

    struct ScriptedConfigurationStore {
        persist_result: Mutex<
            std::result::Result<
                ConfigurationApplicationPersistence,
                ConfigurationApplicationStoreError,
            >,
        >,
        status_outcomes: Mutex<Vec<std::result::Result<(), ConfigurationApplicationStoreError>>>,
        statuses: Mutex<Vec<ConfigurationApplicationStatus>>,
        accept_calls: Mutex<usize>,
        persist_calls: Mutex<usize>,
    }

    #[async_trait]
    impl ConfigurationApplicationStore for ScriptedConfigurationStore {
        async fn accept_configuration_application(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            *self.accept_calls.lock().unwrap() += 1;
            Ok(ConfigurationApplicationPersistence::Persisted)
        }

        async fn persist_accepted_configuration(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            *self.persist_calls.lock().unwrap() += 1;
            self.persist_result.lock().unwrap().clone()
        }

        async fn configuration_application_status(
            &self,
            _track_id: &TrackId,
        ) -> std::result::Result<ConfigurationApplicationStatus, ConfigurationApplicationStoreError>
        {
            Ok(ConfigurationApplicationStatus::Idle)
        }

        async fn update_configuration_application_status(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
            status: ConfigurationApplicationStatus,
        ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
            self.statuses.lock().unwrap().push(status);
            self.status_outcomes.lock().unwrap().remove(0)
        }
    }

    #[async_trait]
    impl ConfigurationApplicationStore for CompletionRecordingConfigurationStore {
        async fn accept_configuration_application(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            if let Some(queue) = &self.assert_queue_empty {
                assert!(
                    queue.claim_next().is_none(),
                    "effects must not enter the queue before configuration persistence"
                );
            }
            *self.persist_calls.lock().unwrap() += 1;
            Ok(self.persist_outcomes.lock().unwrap().remove(0))
        }

        async fn persist_accepted_configuration(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
        ) -> std::result::Result<
            ConfigurationApplicationPersistence,
            ConfigurationApplicationStoreError,
        > {
            if let Some(queue) = &self.assert_queue_empty {
                assert!(
                    queue.claim_next().is_none(),
                    "effects must not enter the queue before configuration persistence"
                );
            }
            *self.persist_calls.lock().unwrap() += 1;
            Ok(self.persist_outcomes.lock().unwrap().remove(0))
        }

        async fn configuration_application_status(
            &self,
            _track_id: &TrackId,
        ) -> std::result::Result<ConfigurationApplicationStatus, ConfigurationApplicationStoreError>
        {
            Ok(ConfigurationApplicationStatus::Idle)
        }

        async fn update_configuration_application_status(
            &self,
            _track_id: &TrackId,
            _request: &ConfigurationApplicationRequest,
            status: ConfigurationApplicationStatus,
        ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
            self.completed_statuses.lock().unwrap().push(status);
            self.complete_outcomes.lock().unwrap().remove(0)
        }
    }

    fn configuration_request(
        candidate: poise_core::track::TrackDefinition,
    ) -> ConfigurationApplicationRequest {
        ConfigurationApplicationRequest {
            application_id: "application-1".to_string(),
            request_hash: "hash-1".to_string(),
            expected_version: "v1".to_string(),
            candidate_version: "v2".to_string(),
            candidate,
        }
    }

    fn changed_candidate(definition: &TrackDefinition) -> TrackDefinition {
        let mut config = definition.track_config().clone();
        config.notional_per_unit *= 2.0;
        TrackDefinition::try_new(
            definition.track_id().clone(),
            definition.instrument().clone(),
            config,
            Some(definition.max_notional()),
            definition.loss_limits().clone(),
            Some(definition.tick_timeout_secs()),
        )
        .unwrap()
    }

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

    #[derive(Default)]
    struct MutableCapacityGuard(Mutex<Option<f64>>);

    impl MutableCapacityGuard {
        fn set(&self, available_notional: Option<f64>) {
            *self.0.lock().unwrap() = available_notional;
        }
    }

    impl AccountCapacityGuard for MutableCapacityGuard {
        fn available_notional_for(&self, _instrument: &Instrument) -> Option<f64> {
            *self.0.lock().unwrap()
        }
    }

    struct BlockingFailingMutationStore {
        entered: Semaphore,
        release: Semaphore,
    }

    impl BlockingFailingMutationStore {
        fn new() -> Self {
            Self {
                entered: Semaphore::new(0),
                release: Semaphore::new(0),
            }
        }

        async fn wait_until_entered(&self) {
            let permit = tokio::time::timeout(Duration::from_secs(1), self.entered.acquire())
                .await
                .expect("mutation store was not entered")
                .expect("entered semaphore closed");
            permit.forget();
        }

        fn release_failure(&self) {
            self.release.add_permits(1);
        }
    }

    #[async_trait]
    impl TrackMutationStore for BlockingFailingMutationStore {
        async fn record_track_activity(
            &self,
            _activity: &crate::StoredTrackActivity,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn commit_track_transition(
            &self,
            _id: &str,
            _control_state: Option<&TrackControlState>,
            _events: &[poise_core::events::DomainEvent],
        ) -> anyhow::Result<CommittedTrackWrite> {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release semaphore closed")
                .forget();
            anyhow::bail!("intentional persistence failure")
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn save_track_control_state(
            &self,
            _track_id: &TrackId,
            _state: &TrackControlState,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn insert_track_pnl_record(
            &self,
            _track_id: &TrackId,
            _record: &TrackPnlRecord,
        ) -> anyhow::Result<bool> {
            Ok(true)
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
                    quantity: PositionQty::new(0.1).unwrap(),
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::New,
                })
                .collect(),
        )
    }

    fn test_executor(
        repository: Arc<MemoryRepository>,
        observer: Arc<RecordingRecoveryAnomalyObserver>,
    ) -> TradingRuntime {
        let (notifications, _) = broadcast::channel(16);
        TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository.clone(),
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        )
    }

    #[tokio::test]
    async fn configuration_application_does_not_install_or_enqueue_when_persistence_fails() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let runtime = test_executor(repository.clone(), observer);
        let track_id = TrackId::new("btc-core");
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(FakeConfigurationStore {
            outcome: Err(ConfigurationApplicationStoreError::WriteFailed(
                "disk full".to_string(),
            )),
            calls: Mutex::new(0),
        });

        let result = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(before.definition.clone()),
                store.clone(),
            )
            .await;

        assert!(matches!(
            result,
            Err(ConfigurationApplicationError::Persistence(
                ConfigurationApplicationStoreError::WriteFailed(_)
            ))
        ));
        assert_eq!(*store.calls.lock().unwrap(), 1);
        assert_eq!(
            runtime
                .committed_track_snapshot(&track_id)
                .await
                .unwrap()
                .unwrap()
                .runtime,
            before.runtime
        );
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn already_persisted_application_never_replays_an_installed_definition() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::AlreadyPersisted),
            calls: Mutex::new(0),
        });

        let receipt = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(before.definition),
                store.clone(),
            )
            .await
            .unwrap();

        assert_eq!(
            receipt.persistence,
            ConfigurationApplicationPersistence::AlreadyPersisted
        );
        assert_eq!(*store.calls.lock().unwrap(), 1);
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn same_application_id_returns_the_committed_result_without_repeating_side_effects() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let request = configuration_request(changed_candidate(&before.definition));
        let store = Arc::new(CompletionRecordingConfigurationStore {
            persist_outcomes: Mutex::new(vec![
                ConfigurationApplicationPersistence::Persisted,
                ConfigurationApplicationPersistence::Persisted,
                ConfigurationApplicationPersistence::AlreadyPersisted,
            ]),
            complete_outcomes: Mutex::new(vec![Ok(()), Ok(()), Ok(()), Ok(())]),
            completed_statuses: Mutex::new(Vec::new()),
            persist_calls: Mutex::new(0),
            assert_queue_empty: None,
        });

        let first = runtime
            .apply_configuration_application(&track_id, request.clone(), store.clone())
            .await
            .unwrap();
        let retry = runtime
            .apply_configuration_application(&track_id, request, store.clone())
            .await
            .unwrap();

        assert_eq!(first.status, retry.status);
        assert_eq!(first.status, ConfigurationApplicationStatus::Active);
        assert_eq!(
            *store.persist_calls.lock().unwrap(),
            3,
            "the retry reads durable idempotency but does not rewrite the candidate"
        );
    }

    #[tokio::test]
    async fn active_audit_retry_only_repairs_audit_without_repeating_application_effects() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let request = configuration_request(changed_candidate(&before.definition));
        let store = Arc::new(ScriptedConfigurationStore {
            persist_result: Mutex::new(Ok(ConfigurationApplicationPersistence::Persisted)),
            status_outcomes: Mutex::new(vec![
                Ok(()),
                Ok(()),
                Err(ConfigurationApplicationStoreError::WriteFailed(
                    "audit unavailable".to_string(),
                )),
                Ok(()),
                Ok(()),
            ]),
            statuses: Mutex::new(Vec::new()),
            accept_calls: Mutex::new(0),
            persist_calls: Mutex::new(0),
        });

        let first = runtime
            .apply_configuration_application(&track_id, request.clone(), store.clone())
            .await;
        assert!(matches!(
            first,
            Err(ConfigurationApplicationError::Persistence(
                ConfigurationApplicationStoreError::WriteFailed(_)
            ))
        ));

        let retry = runtime
            .apply_configuration_application(&track_id, request, store.clone())
            .await
            .unwrap();

        assert_eq!(retry.status, ConfigurationApplicationStatus::Active);
        assert_eq!(*store.accept_calls.lock().unwrap(), 1);
        assert_eq!(*store.persist_calls.lock().unwrap(), 1);
        assert_eq!(
            *store.statuses.lock().unwrap(),
            vec![
                ConfigurationApplicationStatus::WaitingForOrders,
                ConfigurationApplicationStatus::Activating,
                ConfigurationApplicationStatus::Active,
                ConfigurationApplicationStatus::Activating,
                ConfigurationApplicationStatus::Active,
            ]
        );
    }

    #[tokio::test]
    async fn already_persisted_different_definition_blocks_without_mutating_runtime() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::AlreadyPersisted),
            calls: Mutex::new(0),
        });

        let result = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&before.definition)),
                store.clone(),
            )
            .await;

        assert!(matches!(
            result,
            Err(ConfigurationApplicationError::AlreadyPersistedNotInstalled)
        ));
        assert_eq!(*store.calls.lock().unwrap(), 1);
        assert_eq!(
            runtime
                .committed_track_snapshot(&track_id)
                .await
                .unwrap()
                .unwrap()
                .runtime,
            before.runtime
        );
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn configuration_application_repairs_missing_active_audit_for_already_installed_definition()
     {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let previous_definition = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let candidate = changed_candidate(&previous_definition);
        let request = configuration_request(candidate);
        let store = Arc::new(CompletionRecordingConfigurationStore {
            persist_outcomes: Mutex::new(vec![
                ConfigurationApplicationPersistence::Persisted,
                ConfigurationApplicationPersistence::AlreadyPersisted,
                ConfigurationApplicationPersistence::AlreadyPersisted,
                ConfigurationApplicationPersistence::AlreadyPersisted,
            ]),
            complete_outcomes: Mutex::new(vec![
                Ok(()),
                Err(ConfigurationApplicationStoreError::WriteFailed(
                    "audit temporarily unavailable".to_string(),
                )),
                Ok(()),
                Ok(()),
                Ok(()),
            ]),
            completed_statuses: Mutex::new(Vec::new()),
            persist_calls: Mutex::new(0),
            assert_queue_empty: None,
        });

        let first = runtime
            .apply_configuration_application(&track_id, request.clone(), store.clone())
            .await;
        assert!(matches!(
            first,
            Err(ConfigurationApplicationError::Persistence(_))
        ));
        assert!(repository.pending_effects().is_empty());

        let second = runtime
            .apply_configuration_application(&track_id, request, store.clone())
            .await
            .unwrap();

        assert_eq!(
            second.status,
            ConfigurationApplicationStatus::WaitingForOrders
        );
        assert_eq!(
            second.persistence,
            ConfigurationApplicationPersistence::AlreadyPersisted
        );
        assert_eq!(*store.persist_calls.lock().unwrap(), 1);
        assert_eq!(
            *store.completed_statuses.lock().unwrap(),
            vec![
                ConfigurationApplicationStatus::WaitingForOrders,
                ConfigurationApplicationStatus::Activating,
                ConfigurationApplicationStatus::WaitingForOrders,
            ]
        );
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn configuration_application_rejects_terminated_but_accepts_active_binding_after_validation()
     {
        let cases = ["terminated", "active_binding"];
        for case in cases {
            let repository = Arc::new(MemoryRepository::default());
            let runtime = test_executor(
                repository,
                Arc::new(RecordingRecoveryAnomalyObserver::default()),
            );
            let track_id = TrackId::new("btc-core");
            if case == "terminated" {
                runtime
                    .handle(&track_id, TrackInput::Command(TrackCommand::Terminate))
                    .await
                    .unwrap();
            } else {
                runtime
                    .handle(
                        &track_id,
                        TrackInput::Market {
                            observation: MarketObservation::ExecutionQuote {
                                execution_quote: ExecutionQuote {
                                    best_bid: 94.9,
                                    best_ask: 95.1,
                                },
                            },
                            occurred_at: Utc::now(),
                        },
                    )
                    .await
                    .unwrap();
            }
            let candidate = runtime
                .committed_track_snapshot(&track_id)
                .await
                .unwrap()
                .unwrap()
                .definition;
            let store = Arc::new(FakeConfigurationStore {
                outcome: Ok(ConfigurationApplicationPersistence::Persisted),
                calls: Mutex::new(0),
            });

            let result = runtime
                .apply_configuration_application(
                    &track_id,
                    configuration_request(candidate),
                    store.clone(),
                )
                .await;

            assert_eq!(
                *store.calls.lock().unwrap(),
                usize::from(case == "active_binding"),
                "case {case}"
            );
            assert!(matches!(
                result,
                Err(ConfigurationApplicationError::Terminated)
                    | Ok(ConfigurationApplicationReceipt {
                        status: ConfigurationApplicationStatus::WaitingForOrders,
                        ..
                    })
            ));
        }
    }

    #[tokio::test]
    async fn configuration_application_accepts_unresolved_submit_and_waits_for_resolution() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::MarkPrice { mark_price: 95.0 },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::MarkPrice { mark_price: 95.0 },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let queue = runtime.session_effect_queue();
        let pending = queue
            .claim_next()
            .expect("market decision should enqueue a submit effect");
        let TrackEffect::SubmitOrder { request, .. } = pending.effect else {
            panic!("expected a submit effect");
        };
        runtime
            .manager
            .write()
            .await
            .clear_executor_bindings_for_test(&track_id)
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::SubmitOutcomeUnknown {
                    effect_id: pending.effect_id,
                    client_order_id: request.client_order_id,
                    initial_snapshot_confirmed: true,
                },
            )
            .await
            .unwrap();
        assert!(
            !runtime
                .manager
                .read()
                .await
                .get_track(track_id.as_str())
                .unwrap()
                .has_executor_bindings()
        );
        let previous_definition = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let candidate = changed_candidate(&previous_definition);
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        let result = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate),
                store.clone(),
            )
            .await;

        assert!(matches!(
            result,
            Ok(ConfigurationApplicationReceipt {
                status: ConfigurationApplicationStatus::WaitingForOrders,
                ..
            })
        ));
        assert_eq!(*store.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn configuration_application_rejects_a_different_request_while_waiting_without_second_accept()
     {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let definition = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&definition)),
                store.clone(),
            )
            .await
            .unwrap();
        let mut different_request = configuration_request(changed_candidate(&definition));
        different_request.application_id = "application-2".to_string();
        different_request.request_hash = "hash-2".to_string();

        let result = runtime
            .apply_configuration_application(&track_id, different_request, store.clone())
            .await;

        assert!(matches!(
            result,
            Err(ConfigurationApplicationError::ApplicationInProgress)
        ));
        assert_eq!(
            *store.calls.lock().unwrap(),
            1,
            "the waiting application is the only request accepted by the store"
        );
    }

    #[tokio::test]
    async fn configuration_application_waits_for_unresolved_submit_after_empty_complete_snapshot() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::MarkPrice { mark_price: 95.0 },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let queued_submit = runtime
            .session_effect_queue()
            .claim_next()
            .expect("market decision should enqueue a submit effect");
        let TrackEffect::SubmitOrder { request, .. } = queued_submit.effect else {
            panic!("expected submit effect");
        };
        runtime
            .manager
            .write()
            .await
            .clear_executor_bindings_for_test(&track_id)
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::SubmitOutcomeUnknown {
                    effect_id: queued_submit.effect_id,
                    client_order_id: request.client_order_id,
                    initial_snapshot_confirmed: false,
                },
            )
            .await
            .unwrap();
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let candidate = changed_candidate(&before.definition);
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        let receipt = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate.clone()),
                store.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.status,
            ConfigurationApplicationStatus::WaitingForOrders
        );
        let token = runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::SyncExchangeState {
                    token,
                    position: PositionObservation {
                        qty: PositionQty::default(),
                        unrealized_pnl: 0.0,
                    },
                    open_orders: CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![]),
                    reconcile: false,
                },
            )
            .await
            .unwrap();

        let track = runtime
            .manager
            .read()
            .await
            .get_track(track_id.as_str())
            .cloned()
            .unwrap();
        assert_eq!(track.definition(), &before.definition);
        assert!(track.configuration_application_pending_for_test());
        assert_eq!(
            *store.calls.lock().unwrap(),
            1,
            "an unresolved submit blocks candidate persistence and activation"
        );
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.8,
                            best_ask: 95.2,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        assert!(runtime.session_effect_queue().claim_next().is_none());
    }

    #[tokio::test]
    async fn configuration_application_rejects_recovery_anomaly_without_active_binding_before_calling_port()
     {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository,
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .manager
            .write()
            .await
            .set_recovery_anomaly_for_test(&track_id, Some(RecoveryAnomaly::UnknownLiveOrder))
            .unwrap();
        let previous_definition = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let candidate = changed_candidate(&previous_definition);
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        let result = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate),
                store.clone(),
            )
            .await;

        assert!(matches!(
            result,
            Err(ConfigurationApplicationError::RecoveryAnomaly)
        ));
        assert_eq!(*store.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn configuration_application_keeps_paused_control_without_creating_submit() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(&track_id, TrackInput::Command(TrackCommand::Pause))
            .await
            .unwrap();
        assert_eq!(
            repository.activities()[0].kind,
            crate::TrackActivityKind::CommandPause
        );
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&before.definition)),
                store.clone(),
            )
            .await
            .unwrap();

        let manager = runtime.manager.read().await;
        assert!(
            manager
                .get_track(track_id.as_str())
                .unwrap()
                .runtime_state()
                .is_paused()
        );
        assert_eq!(*store.calls.lock().unwrap(), 2);
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn configuration_application_keeps_manual_flatten_without_creating_automatic_submit() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository.clone(),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(&track_id, TrackInput::Command(TrackCommand::Flatten))
            .await
            .unwrap();
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&before.definition)),
                store.clone(),
            )
            .await
            .unwrap();

        let manager = runtime.manager.read().await;
        let track = manager.get_track(track_id.as_str()).unwrap();
        assert_eq!(track.manual_target_override(), Some(Exposure(0.0)));
        assert_eq!(*store.calls.lock().unwrap(), 2);
        assert!(repository.pending_effects().is_empty());
    }

    #[tokio::test]
    async fn configuration_application_persists_before_installing_live_automatic_effects() {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository,
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Position(PositionObservation {
                    qty: PositionQty::new(2.0).unwrap(),
                    unrealized_pnl: 0.0,
                }),
            )
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap();
        let queue = runtime.session_effect_queue();
        queue.clear_track(&track_id);
        runtime
            .manager
            .write()
            .await
            .clear_executor_bindings_for_test(&track_id)
            .unwrap();
        let candidate = changed_candidate(&before.definition);
        let store = Arc::new(CompletionRecordingConfigurationStore {
            persist_outcomes: Mutex::new(vec![
                ConfigurationApplicationPersistence::Persisted,
                ConfigurationApplicationPersistence::Persisted,
            ]),
            complete_outcomes: Mutex::new(vec![Ok(()), Ok(()), Ok(())]),
            completed_statuses: Mutex::new(Vec::new()),
            persist_calls: Mutex::new(0),
            assert_queue_empty: Some(queue.clone()),
        });

        runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate.clone()),
                store.clone(),
            )
            .await
            .unwrap();

        let manager = runtime.manager.read().await;
        let track = manager.get_track(track_id.as_str()).unwrap();
        assert_eq!(track.definition(), &candidate);
        // The seeded linear instrument uses a $100 contract multiplier. At a
        // $750 notional-per-unit candidate, one exposure unit is 7.5 contracts,
        // so the observed 2-contract position is 2 / 7.5 units.
        assert_eq!(track.current_exposure(), &Exposure(2.0 / 7.5));
        assert!(track.inventory_decision().is_some());
        assert!(
            track.fresh_acquisition_pending(),
            "the candidate starts a new risk-acquisition session rather than inheriting the old one"
        );
        assert_eq!(*store.persist_calls.lock().unwrap(), 2);
        assert_eq!(
            *store.completed_statuses.lock().unwrap(),
            vec![
                ConfigurationApplicationStatus::WaitingForOrders,
                ConfigurationApplicationStatus::Activating,
                ConfigurationApplicationStatus::Active,
            ]
        );
    }

    #[tokio::test]
    async fn configuration_application_waiting_supersedes_queued_submit_and_blocks_new_market_submit()
     {
        let repository = Arc::new(MemoryRepository::default());
        let runtime = test_executor(
            repository,
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let candidate = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });

        let receipt = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate.clone()),
                store,
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.status,
            ConfigurationApplicationStatus::WaitingForOrders
        );
        assert!(runtime.session_effect_queue().claim_next().is_none());

        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.8,
                            best_ask: 95.2,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        assert!(runtime.session_effect_queue().claim_next().is_none());
    }

    #[tokio::test]
    async fn configuration_application_barrier_does_not_activate_while_live_binding_remains() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::MarkPrice { mark_price: 95.0 },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let submit = runtime.session_effect_queue().claim_next().unwrap();
        let submit_effect_id = submit.effect_id.clone();
        let TrackEffect::SubmitOrder {
            request,
            desired_exposure,
            ..
        } = submit.effect
        else {
            panic!("expected submit")
        };
        runtime
            .handle(
                &track_id,
                TrackInput::SubmitCompleted {
                    effect_id: submit_effect_id.clone(),
                    request: request.clone(),
                    desired_exposure,
                    receipt: OrderReceipt {
                        order_id: "old-order".into(),
                        client_order_id: request.client_order_id.clone(),
                        filled_qty: PositionQty::default(),
                        status: OrderStatus::New,
                    },
                },
            )
            .await
            .unwrap();
        runtime
            .session_effect_queue()
            .record_outcome(&submit.effect_id, SessionEffectOutcome::Finished);
        assert!(
            !runtime
                .session_effect_queue()
                .has_unresolved_submit_outcome_for_track(&track_id)
        );
        let previous_definition = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let candidate = changed_candidate(&previous_definition);
        let store = Arc::new(FakeConfigurationStore {
            outcome: Ok(ConfigurationApplicationPersistence::Persisted),
            calls: Mutex::new(0),
        });
        runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(candidate.clone()),
                store.clone(),
            )
            .await
            .unwrap();
        let cancel = runtime
            .session_effect_queue()
            .claim_next()
            .expect("configuration wait should dispatch the old-order cancel");
        assert!(matches!(cancel.effect, TrackEffect::CancelOrder { .. }));
        let token = runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        runtime
            .handle(
                &track_id,
                TrackInput::SyncExchangeState {
                    token,
                    position: PositionObservation {
                        qty: PositionQty::default(),
                        unrealized_pnl: 0.0,
                    },
                    open_orders: CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![
                        OrderObservation {
                            order_id: "old-order".into(),
                            client_order_id: request.client_order_id.clone(),
                            side: request.side,
                            price: request.price,
                            quantity: request.quantity,
                            filled_qty: PositionQty::default(),
                            status: OrderStatus::New,
                        },
                    ]),
                    reconcile: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            *store.calls.lock().unwrap(),
            1,
            "live binding blocks file persistence"
        );
        let token = runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        let empty_snapshot = runtime
            .handle(
                &track_id,
                TrackInput::SyncExchangeState {
                    token,
                    position: PositionObservation {
                        qty: PositionQty::default(),
                        unrealized_pnl: 0.0,
                    },
                    open_orders: CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![]),
                    reconcile: false,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            empty_snapshot,
            TrackUpdateReceipt::ExchangeStateSynced(ExchangeStateSyncOutcome::Applied)
        ));
        let track = runtime
            .manager
            .read()
            .await
            .get_track(track_id.as_str())
            .cloned()
            .unwrap();
        assert_eq!(track.definition(), &candidate);
        assert!(!track.configuration_application_pending_for_test());
        assert_eq!(
            *store.calls.lock().unwrap(),
            2,
            "empty complete snapshot persists and activates the candidate"
        );
    }

    #[tokio::test]
    async fn barrier_persist_failure_restores_old_definition_after_authoritative_empty_snapshot() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        let submit = runtime.session_effect_queue().claim_next().unwrap();
        let submit_effect_id = submit.effect_id.clone();
        let TrackEffect::SubmitOrder {
            request,
            desired_exposure,
            ..
        } = submit.effect
        else {
            panic!("expected submit")
        };
        runtime
            .handle(
                &track_id,
                TrackInput::SubmitCompleted {
                    effect_id: submit_effect_id.clone(),
                    request: request.clone(),
                    desired_exposure,
                    receipt: OrderReceipt {
                        order_id: "old-order".into(),
                        client_order_id: request.client_order_id.clone(),
                        filled_qty: PositionQty::default(),
                        status: OrderStatus::New,
                    },
                },
            )
            .await
            .unwrap();
        runtime
            .session_effect_queue()
            .record_outcome(&submit_effect_id, SessionEffectOutcome::Finished);
        let previous = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let store = Arc::new(ScriptedConfigurationStore {
            persist_result: Mutex::new(Err(ConfigurationApplicationStoreError::ExternalChange)),
            status_outcomes: Mutex::new(vec![
                Ok(()),
                Ok(()),
                Err(ConfigurationApplicationStoreError::WriteFailed(
                    "terminal audit unavailable".to_string(),
                )),
            ]),
            statuses: Mutex::new(Vec::new()),
            accept_calls: Mutex::new(0),
            persist_calls: Mutex::new(0),
        });
        let receipt = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&previous)),
                store.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.status,
            ConfigurationApplicationStatus::WaitingForOrders
        );
        let token = runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        let result = runtime
            .handle(
                &track_id,
                TrackInput::SyncExchangeState {
                    token,
                    position: PositionObservation {
                        qty: PositionQty::default(),
                        unrealized_pnl: 0.0,
                    },
                    open_orders: CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![]),
                    reconcile: false,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            runtime
                .committed_track_snapshot(&track_id)
                .await
                .unwrap()
                .unwrap()
                .definition,
            previous,
        );
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.8,
                            best_ask: 95.2,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        while let Some(effect) = runtime.session_effect_queue().claim_next() {
            assert!(
                !matches!(effect.effect, TrackEffect::SubmitOrder { .. }),
                "terminal audit failure re-closes the submit gate and supersedes its new effect"
            );
        }
        assert_eq!(
            *store.statuses.lock().unwrap(),
            vec![
                ConfigurationApplicationStatus::WaitingForOrders,
                ConfigurationApplicationStatus::Activating,
                ConfigurationApplicationStatus::Failed,
            ]
        );
    }

    #[tokio::test]
    async fn post_rename_durability_failure_keeps_gate_closed_and_audit_activating() {
        let runtime = test_executor(
            Arc::new(MemoryRepository::default()),
            Arc::new(RecordingRecoveryAnomalyObserver::default()),
        );
        let track_id = TrackId::new("btc-core");
        let previous = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .definition;
        let store = Arc::new(ScriptedConfigurationStore {
            persist_result: Mutex::new(Err(
                ConfigurationApplicationStoreError::FileReplacedButDurabilityUncertain(
                    "directory sync failed".to_string(),
                ),
            )),
            status_outcomes: Mutex::new(vec![Ok(()), Ok(())]),
            statuses: Mutex::new(Vec::new()),
            accept_calls: Mutex::new(0),
            persist_calls: Mutex::new(0),
        });

        let result = runtime
            .apply_configuration_application(
                &track_id,
                configuration_request(changed_candidate(&previous)),
                store.clone(),
            )
            .await;

        assert!(matches!(
            result,
            Err(ConfigurationApplicationError::Persistence(
                ConfigurationApplicationStoreError::FileReplacedButDurabilityUncertain(_)
            ))
        ));
        assert_eq!(
            runtime
                .committed_track_snapshot(&track_id)
                .await
                .unwrap()
                .unwrap()
                .definition,
            previous
        );
        assert_eq!(
            *store.statuses.lock().unwrap(),
            vec![
                ConfigurationApplicationStatus::WaitingForOrders,
                ConfigurationApplicationStatus::Activating,
            ]
        );
        runtime
            .handle(
                &track_id,
                TrackInput::Market {
                    observation: MarketObservation::ExecutionQuote {
                        execution_quote: ExecutionQuote {
                            best_bid: 94.9,
                            best_ask: 95.1,
                        },
                    },
                    occurred_at: Utc::now(),
                },
            )
            .await
            .unwrap();
        while let Some(effect) = runtime.session_effect_queue().claim_next() {
            assert!(!matches!(effect.effect, TrackEffect::SubmitOrder { .. }));
        }
    }

    fn manager_with_committed_inventory_decision() -> (
        poise_engine::manager::TrackManager,
        InventoryDecisionSnapshot,
    ) {
        let mut manager = seeded_manager();
        manager
            .observe_market_mutation(
                &TrackId::new("btc-core"),
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
                chrono::Utc::now(),
            )
            .unwrap();
        let decision = manager
            .get_track("btc-core")
            .unwrap()
            .inventory_decision()
            .unwrap()
            .clone();
        (manager, decision)
    }

    fn frame_with_recovery_anomaly(active: bool) -> TrackRuntime {
        let manager = seeded_manager();
        let mut snapshot = manager.get_track("btc-core").unwrap().clone();
        snapshot.set_recovery_anomaly(active.then_some(RecoveryAnomaly::UnknownLiveOrder));
        snapshot
    }

    #[tokio::test]
    async fn commit_track_mutation_notifies_recovery_anomaly_activation_edges_only() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer.clone());

        let previous_state = frame_with_recovery_anomaly(false);
        let mut next_state = previous_state.clone();
        next_state.set_exposure_state(
            poise_core::types::Exposure(1.0),
            previous_state.desired_exposure().cloned(),
        );

        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        let mutation_guard = executor
            .commit_track_mutation(
                mutation_guard,
                "btc-core",
                &previous_state,
                &next_state,
                &(),
                None,
                false,
                None,
            )
            .await
            .unwrap();
        drop(mutation_guard);

        assert!(observer.recorded().is_empty());

        let next_state = frame_with_recovery_anomaly(true);
        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        executor
            .commit_track_mutation(
                mutation_guard,
                "btc-core",
                &previous_state,
                &next_state,
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
    async fn live_only_executor_change_notifies_recovery_anomaly_edge() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let mut receiver = notifications.subscribe();
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository,
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer.clone(),
        );
        let previous_state = frame_with_recovery_anomaly(false);
        let next_state = frame_with_recovery_anomaly(true);

        executor.notify_live_only_executor_changes("btc-core", &previous_state, &next_state);

        assert_eq!(observer.recorded(), vec![("btc-core".to_string(), true)]);
        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
    }

    #[tokio::test]
    async fn commit_track_mutation_notifies_recovery_anomaly_clear_edges_only() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer.clone());

        let previous_state = frame_with_recovery_anomaly(true);
        let next_state = frame_with_recovery_anomaly(false);

        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        executor
            .commit_track_mutation(
                mutation_guard,
                "btc-core",
                &previous_state,
                &next_state,
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
    async fn executor_view_change_notifies_without_durable_work() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let mut receiver = notifications.subscribe();
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository.clone(),
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer.clone(),
        );
        let previous_state = frame_with_recovery_anomaly(true);
        let mut next_state = previous_state.clone();
        next_state.set_recovery_anomaly(Some(RecoveryAnomaly::DuplicateLiveOrders));

        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        executor
            .commit_track_mutation(
                mutation_guard,
                "btc-core",
                &previous_state,
                &next_state,
                &(),
                None,
                true,
                None,
            )
            .await
            .unwrap();

        assert!(observer.recorded().is_empty());
        assert_eq!(repository.business_write_call_count(), 0);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("executor view change should notify subscribers")
                .unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
    }

    #[tokio::test]
    async fn trading_runtime_exposes_market_data_health_deadline() {
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
    async fn exchange_sync_applies_only_for_the_matching_observation_revision() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token.clone(),
                PositionObservation {
                    qty: PositionQty::new(0.75).unwrap(),
                    unrealized_pnl: 12.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ExchangeStateSyncOutcome::Applied));
        let snapshot = executor
            .manager()
            .read()
            .await
            .get_track("btc-core")
            .cloned()
            .unwrap();
        assert!(snapshot.current_exposure().0 > 0.0);
        assert_eq!(snapshot.unrealized_pnl(), 12.0);

        let reused = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        assert!(matches!(reused, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn anomalous_exchange_snapshot_keeps_submitted_effect_ownership() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        let track_id = TrackId::new("btc-core");
        services
            .runtime
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
        let claimed = services
            .session_effect_queue
            .claim_next()
            .expect("test requires a submit effect");
        let effect_id = claimed.effect_id.clone();
        let TrackEffect::SubmitOrder {
            request,
            recovery_token,
            ..
        } = claimed.effect
        else {
            panic!("first effect should be a submit");
        };
        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                track_id.as_str(),
                &effect_id,
                recovery_token,
                SubmitRecoveryOrderLookup::NotQueried,
            )
            .await
            .unwrap();
        assert!(matches!(
            attempt,
            crate::submit_effect_service::SubmitAttempt::Dispatch(_)
        ));
        assert!(
            services
                .session_effect_queue
                .record_submit_awaiting_resolution(&effect_id)
        );

        services
            .runtime
            .observe_market(
                track_id.as_str(),
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 84.9,
                        best_ask: 85.1,
                    },
                },
            )
            .await
            .unwrap();
        let token = services
            .runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        let open_orders = CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![
            OrderObservation {
                order_id: "protected-live-order".to_string(),
                client_order_id: request.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                filled_qty: PositionQty::default(),
                status: OrderStatus::New,
            },
            OrderObservation {
                order_id: "unknown-live-order".to_string(),
                client_order_id: "unknown-client".to_string(),
                side: Side::Sell,
                price: 80.0,
                quantity: PositionQty::new(3.0).unwrap(),
                filled_qty: PositionQty::default(),
                status: OrderStatus::New,
            },
        ]);

        let outcome = services
            .runtime
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                open_orders,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExchangeStateSyncOutcome::Applied));

        let queue = services.session_effect_queue.snapshot_for_track(&track_id);
        let submitted = queue
            .pending_effects
            .iter()
            .find(|effect| effect.effect_id == effect_id)
            .expect("anomalous snapshot must retain submit ownership");
        assert_eq!(
            submitted.state,
            SessionPendingEffectState::SubmitAwaitingResolution
        );
        let state = services
            .submit_effect
            .manager()
            .read()
            .await
            .get_track(track_id.as_str())
            .cloned()
            .expect("track state");
        assert!(state.recovery_anomaly_active());
        let (_, status) = state
            .binding_receipt_for_client_order_id(&request.client_order_id)
            .expect("authorized binding should remain owned");
        assert_eq!(status, BindingStatus::CancelAfterSubmit);
    }

    #[tokio::test]
    async fn matching_order_fact_resolves_submit_owner_and_unblocks_protection_cancel() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);
        let track_id = TrackId::new("btc-core");
        services
            .runtime
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
        let claimed = services
            .session_effect_queue
            .claim_next()
            .expect("test requires a submit effect");
        let effect_id = claimed.effect_id.clone();
        let TrackEffect::SubmitOrder {
            request,
            recovery_token,
            ..
        } = claimed.effect
        else {
            panic!("first effect should be a submit");
        };
        let attempt = services
            .submit_effect
            .recover_or_dispatch(
                track_id.as_str(),
                &effect_id,
                recovery_token,
                SubmitRecoveryOrderLookup::NotQueried,
            )
            .await
            .unwrap();
        assert!(matches!(
            attempt,
            crate::submit_effect_service::SubmitAttempt::Dispatch(_)
        ));
        assert!(
            services
                .session_effect_queue
                .record_submit_awaiting_resolution(&effect_id)
        );
        services.session_effect_queue.record_outcome(
            &effect_id,
            SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            },
        );

        services
            .runtime
            .observe_market(
                track_id.as_str(),
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 84.9,
                        best_ask: 85.1,
                    },
                },
            )
            .await
            .unwrap();
        let (_, unabsorbed) = services
            .runtime
            .observe_order_with_absorb_result(
                track_id.as_str(),
                OrderObservation {
                    order_id: "unmatched-order".to_string(),
                    client_order_id: "other-client".to_string(),
                    side: Side::Sell,
                    price: 80.0,
                    quantity: PositionQty::new(1.0).unwrap(),
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::New,
                },
            )
            .await
            .unwrap();
        assert_eq!(unabsorbed, OrderUpdateAbsorbResult::Unabsorbed);
        assert!(
            services
                .session_effect_queue
                .snapshot_for_track(&track_id)
                .pending_effects
                .iter()
                .any(|effect| effect.effect_id == effect_id),
            "an unrelated order fact must not release submit ownership"
        );
        let (_, absorb_result) = services
            .runtime
            .observe_order_with_absorb_result(
                track_id.as_str(),
                OrderObservation {
                    order_id: "protected-live-order".to_string(),
                    client_order_id: request.client_order_id.clone(),
                    side: request.side,
                    price: request.price,
                    quantity: request.quantity,
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::New,
                },
            )
            .await
            .unwrap();
        assert_eq!(absorb_result, OrderUpdateAbsorbResult::Applied);

        assert!(
            services
                .session_effect_queue
                .snapshot_for_track(&track_id)
                .pending_effects
                .iter()
                .all(|effect| effect.effect_id != effect_id),
            "the matching order fact should retire the unresolved submit owner"
        );
        let cancel = services
            .session_effect_queue
            .claim_next()
            .expect("protection cancel should be unblocked after resolving submit ownership");
        assert!(matches!(
            cancel.effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "protected-live-order"
        ));
    }

    #[tokio::test]
    async fn position_observation_invalidates_an_older_snapshot_even_when_value_repeats() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        executor
            .observe_position(
                "btc-core",
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
            )
            .await
            .unwrap();
        let writes_before_stale_sync = repository.business_write_call_count();
        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::new(1.0).unwrap(),
                    unrealized_pnl: -25.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
        let snapshot = executor
            .manager()
            .read()
            .await
            .get_track("btc-core")
            .cloned()
            .unwrap();
        assert_eq!(snapshot.current_exposure(), &Exposure(0.0));
        assert_eq!(snapshot.unrealized_pnl(), 0.0);
        assert_eq!(
            repository.business_write_call_count(),
            writes_before_stale_sync,
            "a superseded snapshot must not write durable state"
        );
    }

    #[tokio::test]
    async fn order_observation_invalidates_an_older_snapshot_even_when_unabsorbed() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        let (_, absorb_result) = executor
            .observe_order_with_absorb_result(
                "btc-core",
                OrderObservation {
                    order_id: "new-exchange-order".to_string(),
                    client_order_id: "unknown-client".to_string(),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: PositionQty::new(0.1).unwrap(),
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::New,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            absorb_result,
            poise_engine::executor::OrderUpdateAbsorbResult::Unabsorbed
        );

        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn same_state_cancel_receipt_still_invalidates_an_older_snapshot() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        executor
            .record_cancel_order_success(
                "btc-core",
                "cancel-effect",
                "already-absent-order",
                &OrderReceipt {
                    order_id: "already-absent-order".to_string(),
                    client_order_id: "already-terminal-client".to_string(),
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::Canceled,
                },
            )
            .await
            .unwrap();

        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::new(1.0).unwrap(),
                    unrealized_pnl: -10.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn complete_absent_submit_lookup_invalidates_only_after_exchange_was_queried() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let untouched_token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        executor
            .recover_submit_effect(
                "btc-core",
                "submit-effect",
                &SubmitRecoveryToken::empty(),
                &SubmitRecoveryOrderLookup::NotQueried,
            )
            .await
            .unwrap();
        let after_unqueried = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();
        assert_eq!(
            untouched_token, after_unqueried,
            "a direct submit path without an exchange query is not a newer exchange fact"
        );

        executor
            .recover_submit_effect(
                "btc-core",
                "submit-effect",
                &SubmitRecoveryToken::empty(),
                &SubmitRecoveryOrderLookup::Complete(None),
            )
            .await
            .unwrap();
        let outcome = executor
            .sync_exchange_state_without_reconcile(
                after_unqueried,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn cancel_follow_up_created_after_token_is_not_consumed_by_the_older_snapshot() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let track_id = TrackId::new("btc-core");
        let enqueued = executor.session_effect_queue.enqueue_transition_effects(
            &track_id,
            &[TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: "cancelled-order".to_string(),
            }],
            chrono::Utc::now(),
        );
        let effect_id = enqueued.effect_ids()[0].clone();
        executor.session_effect_queue.claim_next().unwrap();
        let token = executor
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();

        executor.session_effect_queue.record_cancel_resolution(
            &effect_id,
            CancelReceiptResolution::Unknown {
                order_id: "cancelled-order".to_string(),
                reason: "timeout".to_string(),
            },
        );
        let before = executor.session_effect_queue.snapshot_for_track(&track_id);
        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
        assert_eq!(
            executor.session_effect_queue.snapshot_for_track(&track_id),
            before,
            "the old empty snapshot must not close a newer cancel follow-up"
        );
        assert_eq!(
            before.pending_effects[0].state,
            SessionPendingEffectState::AwaitingFollowUp
        );
    }

    #[tokio::test]
    async fn rejected_candidate_does_not_make_an_old_exchange_snapshot_token_current_again() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        let mutation: std::result::Result<TrackTransition, super::TrackMutationError> = executor
            .mutate_track("btc-core", |manager| {
                let _ = manager.observe_position_update(
                    &TrackId::new("btc-core"),
                    PositionObservation {
                        qty: PositionQty::default(),
                        unrealized_pnl: 0.0,
                    },
                )?;
                anyhow::bail!("forced candidate rejection")
            })
            .await;
        assert!(mutation.is_err());

        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::new(1.0).unwrap(),
                    unrealized_pnl: -10.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn fresh_start_does_not_reset_the_observation_revision_to_an_old_token() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let token = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();
        let instrument_spec = executor
            .manager()
            .read()
            .await
            .get_track("btc-core")
            .unwrap()
            .instrument_spec()
            .clone();

        assert!(
            executor
                .fresh_start_track_runtime(
                    &TrackId::new("btc-core"),
                    TrackControlState::default(),
                    TrackPnlStats::default(),
                    FreshSessionExternalInputs {
                        current_exposure: Exposure(0.0),
                        position_qty: poise_core::instrument::PositionQty::default(),
                        market_data: None,
                        instrument_spec,
                    },
                )
                .await
                .unwrap()
        );

        let outcome = executor
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::new(1.0).unwrap(),
                    unrealized_pnl: -10.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ExchangeStateSyncOutcome::RevisionChanged));
    }

    #[tokio::test]
    async fn pnl_and_pure_market_updates_do_not_invalidate_exchange_snapshot_tokens() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository, observer);
        let initial = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        executor
            .record_track_pnl(
                "btc-core",
                TrackPnlRecord::funding(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    chrono::Utc::now(),
                    "test:funding".to_string(),
                    Some("test:funding:1".to_string()),
                    amount(-0.25, "USDT"),
                ),
            )
            .await
            .unwrap();
        executor
            .observe_market(
                "btc-core",
                MarketObservation::MarkPrice { mark_price: 100.0 },
            )
            .await
            .unwrap();
        let after_pnl_and_mark = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        assert_eq!(initial, after_pnl_and_mark);

        executor
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
        let after_binding_change = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        assert_ne!(after_pnl_and_mark, after_binding_change);
    }

    #[tokio::test]
    async fn handle_track_pnl_commits_mixed_assets_consistently_and_deduplicates_retry() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let runtime = test_executor(repository.clone(), observer);
        let track_id = TrackId::new("btc-core");
        let occurred_at = Utc::now();

        let usdt_record = TrackPnlRecord::trade_summary(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            occurred_at,
            "test:usdt".to_string(),
            Some("trade:usdt:1".to_string()),
            None,
            amount(5.0, "USDT"),
            Some(amount(0.5, "USDT")),
        );
        let btc_record = TrackPnlRecord::funding(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            occurred_at,
            "test:btc".to_string(),
            Some("trade:btc:1".to_string()),
            amount(0.001, "BTC"),
        );

        assert!(matches!(
            runtime
                .handle(&track_id, TrackInput::TrackPnl(usdt_record))
                .await
                .unwrap(),
            TrackUpdateReceipt::PnlRecorded { inserted: true }
        ));
        assert!(matches!(
            runtime
                .handle(&track_id, TrackInput::TrackPnl(btc_record.clone()))
                .await
                .unwrap(),
            TrackUpdateReceipt::PnlRecorded { inserted: true }
        ));
        assert!(matches!(
            runtime
                .handle(&track_id, TrackInput::TrackPnl(btc_record))
                .await
                .unwrap(),
            TrackUpdateReceipt::PnlRecorded { inserted: false }
        ));

        let committed = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .pnl_stats;
        let persisted = repository
            .load_track_pnl_stats(&track_id, occurred_at.date_naive())
            .await
            .unwrap();
        assert_eq!(committed, persisted);
        assert_eq!(committed.pnl_asset.as_ref().unwrap().as_str(), "USDT");
        assert_eq!(committed.gross_realized_pnl_cumulative, 5.0);
        assert_eq!(committed.unvalued_cashflows_cumulative.len(), 1);
        assert!(
            committed
                .net_realized_pnl_cumulative()
                .complete_value()
                .is_none()
        );
    }

    #[tokio::test]
    async fn handle_track_pnl_keeps_committed_state_when_persistence_fails() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let runtime = test_executor(repository.clone(), observer);
        let track_id = TrackId::new("btc-core");
        let occurred_at = Utc::now();

        runtime
            .handle(
                &track_id,
                TrackInput::TrackPnl(TrackPnlRecord::funding(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    occurred_at,
                    "test:funding".to_string(),
                    Some("funding:1".to_string()),
                    amount(-0.25, "USDT"),
                )),
            )
            .await
            .unwrap();
        let before = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .pnl_stats;

        repository.fail_next_pnl_insert("pnl write failed");
        let error = runtime
            .handle(
                &track_id,
                TrackInput::TrackPnl(TrackPnlRecord::funding(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    occurred_at,
                    "test:funding".to_string(),
                    Some("funding:2".to_string()),
                    amount(-0.5, "USDT"),
                )),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("pnl write failed"));

        let after = runtime
            .committed_track_snapshot(&track_id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .pnl_stats;
        let persisted = repository
            .load_track_pnl_stats(&track_id, occurred_at.date_naive())
            .await
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(persisted, before);
    }

    #[tokio::test]
    async fn observe_market_live_only_tick_does_not_emit_track_changed() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = TradingRuntime::with_session_queue(
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
            let mut updated = track.clone();
            updated.set_runtime_state(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            )));
            updated.set_exposure_state(
                poise_core::types::Exposure(2.4),
                Some(poise_core::types::Exposure(2.4)),
            );
            manager.install_planned_runtime(updated);
        }
        let token_before_tick = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

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
        let first_decision = executor
            .track_runtime_view("btc-core")
            .await
            .unwrap()
            .unwrap()
            .decision;
        let second_transition = executor
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
        let queried_decisions = [
            executor
                .track_runtime_view("btc-core")
                .await
                .unwrap()
                .unwrap()
                .decision,
            executor
                .track_runtime_view("btc-core")
                .await
                .unwrap()
                .unwrap()
                .decision,
        ];
        let token_after_tick = executor
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        assert!(transition.facts.is_empty());
        assert!(transition.effects.is_empty());
        assert!(second_transition.facts.is_empty());
        assert!(second_transition.effects.is_empty());
        assert_eq!(queried_decisions[0], queried_decisions[1]);
        assert_eq!(
            queried_decisions[0].revision,
            Some(first_decision.revision.unwrap() + 1)
        );
        assert_eq!(
            queried_decisions[0].risk_approved_target,
            first_decision.risk_approved_target
        );
        assert_eq!(
            queried_decisions[0].execution_target,
            first_decision.execution_target
        );
        assert_eq!(token_before_tick, token_after_tick);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap()
                .is_none(),
            "live-only tick should not persist control state"
        );
        assert_eq!(
            repository.business_write_call_count(),
            0,
            "live-only tick should not persist any durable runtime projection"
        );
    }

    #[tokio::test]
    async fn empty_transition_commit_skips_all_shared_work() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let mut receiver = notifications.subscribe();
        let queue = SessionEffectQueue::default();
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository.clone(),
            queue.clone(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        );

        let transition = executor
            .refresh_market_data_health("btc-core")
            .await
            .unwrap();

        assert!(transition.facts.is_empty());
        assert!(transition.effects.is_empty());
        assert_eq!(repository.business_write_call_count(), 0);
        assert!(repository.pending_effects().is_empty());
        assert!(
            queue
                .snapshot_for_track(&TrackId::new("btc-core"))
                .pending_effects
                .is_empty()
        );
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn market_data_stale_edge_notifies_without_durable_write() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let mut receiver = notifications.subscribe();
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository.clone(),
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        );
        let previous_state = {
            let manager = executor.manager.read().await;
            manager.get_track("btc-core").cloned().unwrap()
        };
        let mut next_state = previous_state.clone();
        next_state.set_market_data_stale_since(Some(Utc::now()));

        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        executor
            .commit_track_mutation(
                mutation_guard,
                "btc-core",
                &previous_state,
                &next_state,
                &(),
                None,
                true,
                None,
            )
            .await
            .unwrap();

        assert_eq!(repository.business_write_call_count(), 0);
        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
    }

    #[tokio::test]
    async fn account_capacity_refresh_does_not_clear_live_quote() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let guard = Arc::new(MutableCapacityGuard::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            repository.clone(),
            repository,
            SessionEffectQueue::default(),
            notifications,
            guard.clone(),
            observer,
        );
        executor
            .observe_market(
                "btc-core",
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 99.9,
                        best_ask: 100.1,
                    },
                },
            )
            .await
            .unwrap();

        let before = {
            let manager = executor.manager.read().await;
            manager.get_track("btc-core").unwrap().live_quote_state()
        };
        guard.set(Some(0.0));
        {
            let mut manager = executor.manager.write().await;
            executor
                .sync_account_capacity_gate_state(&mut manager, "btc-core")
                .unwrap();
        }
        let after = {
            let manager = executor.manager.read().await;
            manager.get_track("btc-core").unwrap().live_quote_state()
        };

        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn decision_query_waits_for_persistence_while_manager_keeps_committed_state() {
        let (manager, committed_before) = manager_with_committed_inventory_decision();
        let mutation_store = Arc::new(BlockingFailingMutationStore::new());
        let effect_journal = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = TradingRuntime::with_session_queue(
            manager,
            mutation_store.clone(),
            effect_journal,
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        );

        let trading_runtime = executor.clone();
        let mutation = tokio::spawn(async move {
            trading_runtime
                .command("btc-core", poise_engine::command::TrackCommand::Pause)
                .await
        });
        mutation_store.wait_until_entered().await;
        {
            let manager = executor.manager.read().await;
            assert_eq!(
                manager.get_track("btc-core").unwrap().inventory_decision(),
                Some(&committed_before),
                "manager must keep committed state while persistence is pending"
            );
        }

        let query_executor = executor.clone();
        let mut query =
            tokio::spawn(async move { query_executor.track_runtime_view("btc-core").await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut query)
                .await
                .is_err(),
            "decision query must wait on the per-track mutation transaction"
        );

        mutation_store.release_failure();
        assert!(mutation.await.unwrap().is_err());
        let queried_after_failure = query.await.unwrap().unwrap().unwrap();
        assert_eq!(
            queried_after_failure.decision,
            InventoryDecisionView {
                revision: Some(committed_before.revision),
                policy_target: Some(committed_before.policy_target.0),
                risk_approved_target: Some(committed_before.risk_approved_target.0),
                acquisition_frontier: committed_before
                    .acquisition_frontier
                    .as_ref()
                    .map(|frontier| frontier.0),
                execution_target: Some(committed_before.execution_target.0),
                reason: committed_before.reason,
            }
        );
        assert_eq!(
            executor
                .manager
                .read()
                .await
                .get_track("btc-core")
                .unwrap()
                .inventory_decision(),
            Some(&committed_before)
        );
    }

    #[tokio::test]
    async fn market_deadline_query_waits_for_market_persistence() {
        let mutation_store = Arc::new(BlockingFailingMutationStore::new());
        let effect_journal = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = TradingRuntime::with_session_queue(
            seeded_manager(),
            mutation_store.clone(),
            effect_journal,
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        );

        let trading_runtime = executor.clone();
        let mutation = tokio::spawn(async move {
            trading_runtime
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
        });
        mutation_store.wait_until_entered().await;

        let query_executor = executor.clone();
        let mut deadline =
            tokio::spawn(
                async move { query_executor.market_data_health_deadline("btc-core").await },
            );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut deadline)
                .await
                .is_err(),
            "deadline must not observe an uncommitted market tick"
        );

        mutation_store.release_failure();
        assert!(mutation.await.unwrap().is_err());
        assert_eq!(deadline.await.unwrap().unwrap(), None);
    }

    #[tokio::test]
    async fn aborted_caller_cannot_publish_a_speculative_inventory_decision() {
        let (manager, committed_before) = manager_with_committed_inventory_decision();
        let mutation_store = Arc::new(BlockingFailingMutationStore::new());
        let effect_journal = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let (notifications, _) = broadcast::channel(16);
        let executor = TradingRuntime::with_session_queue(
            manager,
            mutation_store.clone(),
            effect_journal,
            SessionEffectQueue::default(),
            notifications,
            Arc::new(NoopGuard),
            observer,
        );

        let trading_runtime = executor.clone();
        let mutation = tokio::spawn(async move {
            trading_runtime
                .command("btc-core", poise_engine::command::TrackCommand::Pause)
                .await
        });
        mutation_store.wait_until_entered().await;
        mutation.abort();
        assert!(mutation.await.unwrap_err().is_cancelled());

        let query_executor = executor.clone();
        let mut query =
            tokio::spawn(async move { query_executor.track_runtime_view("btc-core").await });
        let early_query = tokio::time::timeout(Duration::from_millis(50), &mut query).await;
        mutation_store.release_failure();
        let (query_was_blocked, queried_after_failure) = match early_query {
            Ok(result) => (false, result.unwrap().unwrap().unwrap()),
            Err(_) => (
                true,
                tokio::time::timeout(Duration::from_secs(1), query)
                    .await
                    .expect("query should finish after the detached transaction fails")
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            ),
        };

        assert!(
            query_was_blocked,
            "caller cancellation must not release the mutation owner before commit finishes"
        );
        assert_eq!(
            queried_after_failure.decision,
            InventoryDecisionView {
                revision: Some(committed_before.revision),
                policy_target: Some(committed_before.policy_target.0),
                risk_approved_target: Some(committed_before.risk_approved_target.0),
                acquisition_frontier: committed_before
                    .acquisition_frontier
                    .as_ref()
                    .map(|frontier| frontier.0),
                execution_target: Some(committed_before.execution_target.0),
                reason: committed_before.reason,
            }
        );
        assert_eq!(
            executor
                .manager
                .read()
                .await
                .get_track("btc-core")
                .unwrap()
                .inventory_decision(),
            Some(&committed_before)
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
        assert_eq!(
            repository.business_write_call_count(),
            0,
            "effect status writeback should not create durable track truth"
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
            !transition.effects.is_empty() || !transition.facts.is_empty(),
            "test requires a durable transition"
        );
        assert_eq!(
            repository
                .load_track_control_state(&TrackId::new("btc-core"))
                .await
                .unwrap(),
            Some(TrackControlState::default())
        );
    }

    #[tokio::test]
    async fn observe_market_enqueues_current_session_effects() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository);

        let transition = services
            .runtime
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
    async fn effect_journal_append_failure_warns_without_discarding_current_session_work() {
        let repository = Arc::new(MemoryRepository::default());
        repository.fail_effect_journal_appends("journal disk is read-only");
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        let transition = services
            .runtime
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
            "test requires a session effect"
        );
        assert_eq!(
            repository.business_write_call_count(),
            1,
            "the durable business transition must remain committed"
        );
        assert!(
            repository.pending_effects().is_empty(),
            "a failed best-effort journal append must not be presented as a durable outbox row"
        );
        assert!(
            services.session_effect_queue.claim_next().is_some(),
            "journal failure must not remove executable work from the current session"
        );

        let diagnostics = services.effect_journal_diagnostics.snapshot();
        assert_eq!(diagnostics.failure_count, 1);
        let failure = diagnostics.last_failure.unwrap();
        assert_eq!(failure.track_id, TrackId::new("btc-core"));
        assert_eq!(failure.operation, "append_entries");
        assert!(failure.error.contains("journal disk is read-only"));
    }

    #[tokio::test]
    async fn effect_journal_outcome_failure_is_best_effort_and_warning_stays_sticky() {
        let repository = Arc::new(MemoryRepository::default());
        repository.seed_pending_submit_effect();
        repository.fail_next_effect_journal_outcome("journal outcome write timed out");
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
            .complete_effect_succeeded("btc-core", "btc-core:batch-1:0")
            .await
            .unwrap();
        assert_eq!(
            repository.pending_effects()[0].status,
            EffectStatus::Pending,
            "the diagnostic projection may remain stale when its best-effort write fails"
        );
        let first_diagnostics = services.effect_journal_diagnostics.snapshot();
        assert_eq!(first_diagnostics.failure_count, 1);
        assert_eq!(
            first_diagnostics.last_failure.as_ref().unwrap().operation,
            "record_outcomes"
        );

        services
            .runtime
            .complete_effect_succeeded("btc-core", "btc-core:batch-1:0")
            .await
            .unwrap();
        assert_eq!(
            repository.pending_effects()[0].status,
            EffectStatus::Succeeded
        );
        assert_eq!(
            services.effect_journal_diagnostics.snapshot().failure_count,
            1,
            "a later successful outcome write cannot repair the earlier journal gap"
        );
    }

    #[tokio::test]
    async fn effect_only_transition_enqueues_effect_without_business_write() {
        let repository = Arc::new(MemoryRepository::default());
        let observer = Arc::new(RecordingRecoveryAnomalyObserver::default());
        let executor = test_executor(repository.clone(), observer);
        let (snapshot, runtime) = {
            let manager = executor.manager();
            let manager = manager.read().await;
            (
                manager
                    .get_track("btc-core")
                    .cloned()
                    .expect("seeded track should exist"),
                manager
                    .get_track("btc-core")
                    .cloned()
                    .expect("seeded track should exist"),
            )
        };
        let effect = TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 95.0,
                quantity: PositionQty::new(0.1).unwrap(),
                client_order_id: "effect-only-submit".into(),
                reduce_only: false,
            },
            desired_exposure: Exposure(4.0),
            submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
        };
        let transition = TrackTransition::commit(runtime, Vec::new(), vec![effect]);

        let mutation_guard = executor.lock_track_mutation("btc-core").await;
        executor
            .commit_track_mutation(
                mutation_guard,
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
                        quantity: PositionQty::new(0.1).unwrap(),
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
        let token = services
            .runtime
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        services
            .runtime
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
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
                        quantity: PositionQty::new(0.1).unwrap(),
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
            .runtime
            .prepare_exchange_state_sync(track_id.as_str())
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
        let (services, notifications) = track_write_services(seeded_manager(), repository.clone());
        let track_id = TrackId::new("btc-core");
        services
            .runtime
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
        let revision_before_sync = services
            .runtime
            .track_runtime_view(track_id.as_str())
            .await
            .unwrap()
            .unwrap()
            .decision
            .revision
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
        let token = services
            .runtime
            .prepare_exchange_state_sync(track_id.as_str())
            .await
            .unwrap();
        let mut receiver = notifications.subscribe();

        let outcome = services
            .runtime
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(Vec::new()),
            )
            .await
            .unwrap();
        let ExchangeStateSyncOutcome::Applied = outcome else {
            panic!("current exchange snapshot should apply");
        };

        let decision_after_sync = services
            .runtime
            .track_runtime_view(track_id.as_str())
            .await
            .unwrap()
            .unwrap()
            .decision;
        assert_eq!(
            decision_after_sync.revision,
            Some(revision_before_sync + 1),
            "closed cancel follow-up should force reconcile even when no effect is needed"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("decision-only reconcile should notify subscribers")
                .unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: track_id.clone(),
            }
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
                        quantity: PositionQty::new(0.1).unwrap(),
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
        let token = services
            .runtime
            .prepare_exchange_state_sync("btc-core")
            .await
            .unwrap();

        services
            .runtime
            .sync_exchange_state_without_reconcile(
                token,
                PositionObservation {
                    qty: PositionQty::default(),
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
                &[poise_core::events::DomainEvent::ExposureTargetChanged {
                    from: Exposure(0.0),
                    to: Exposure(1.0),
                }],
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
            let mut updated = track.clone();
            updated.set_runtime_state(TrackState::Running(ControlState::Automatic(
                AutoState::FollowingBand,
            )));
            updated.set_exposure_state(poise_core::types::Exposure(4.0), None);
            let mut pnl_stats = updated.pnl_stats().clone();
            pnl_stats.pnl_utc_day = chrono::Utc::now().date_naive();
            pnl_stats.gross_realized_pnl_today = -290.0;
            pnl_stats.gross_realized_pnl_cumulative = -290.0;
            updated.replace_pnl_stats(pnl_stats);
            updated.set_unrealized_pnl(-20.0);
            manager.install_planned_runtime(updated);
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
            transition.next_state().runtime_state(),
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
    async fn record_cancel_order_success_clears_cancel_binding_without_fill() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
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
        assert!(!pending_submits.is_empty(), "test requires a submit effect");
        let blocked_submit = pending_submits[0].clone();
        let TrackEffect::SubmitOrder {
            request: blocked_request,
            ..
        } = &blocked_submit.effect
        else {
            panic!("blocked effect should be submit order");
        };
        let blocked_client_order_id = blocked_request.client_order_id.clone();
        let closed_order_id = "closed-order";
        let cancel_effect = repository
            .replace_submit_effect_with_cancel_order(&blocked_submit.effect_id, closed_order_id);

        let manager = services.runtime.manager();
        {
            let mut manager = manager.write().await;
            let mut snapshot = manager
                .get_track("btc-core")
                .cloned()
                .expect("track runtime");
            assert!(
                snapshot.set_binding_order_status_for_client_order_id(
                    &blocked_client_order_id,
                    Some(closed_order_id.into()),
                    BindingStatus::CancelPending,
                ),
                "blocked submit binding should exist"
            );
            manager.install_planned_runtime(snapshot);
        }
        let resolution = services
            .runtime
            .record_cancel_order_success(
                "btc-core",
                &cancel_effect.effect_id,
                "closed-order",
                &OrderReceipt {
                    order_id: "closed-order".to_string(),
                    client_order_id: blocked_client_order_id.clone(),
                    filled_qty: PositionQty::default(),
                    status: crate::exchange::OrderStatus::Canceled,
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
            .get_track("btc-core")
            .cloned()
            .expect("track runtime");
        let (_, blocked_binding_status) = snapshot
            .binding_receipt_for_client_order_id(&blocked_client_order_id)
            .expect("blocked binding should remain in runtime history");
        assert_eq!(blocked_binding_status, BindingStatus::Terminal);
        assert!(
            !snapshot.has_active_binding_for_order_id(closed_order_id),
            "closed order should no longer have an active runtime binding"
        );
    }

    #[tokio::test]
    async fn record_cancel_order_success_with_fill_classifies_cancel_receipt() {
        let repository = Arc::new(MemoryRepository::default());
        let (services, _) = track_write_services(seeded_manager(), repository.clone());

        services
            .runtime
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
        assert!(!pending_submits.is_empty(), "test requires a submit effect");
        let blocked_submit = pending_submits[0].clone();
        let TrackEffect::SubmitOrder {
            request: blocked_request,
            ..
        } = &blocked_submit.effect
        else {
            panic!("blocked effect should be submit order");
        };
        let blocked_client_order_id = blocked_request.client_order_id.clone();
        let closed_order_id = "closed-order";
        let cancel_effect = repository
            .replace_submit_effect_with_cancel_order(&blocked_submit.effect_id, closed_order_id);

        let manager = services.runtime.manager();
        {
            let mut manager = manager.write().await;
            let mut snapshot = manager
                .get_track("btc-core")
                .cloned()
                .expect("track runtime");
            assert!(
                snapshot.set_binding_order_status_for_client_order_id(
                    &blocked_client_order_id,
                    Some(closed_order_id.into()),
                    BindingStatus::CancelPending,
                ),
                "blocked submit binding should exist"
            );
            manager.install_planned_runtime(snapshot);
        }
        let outcome_writes_before_cancel = repository.effect_outcome_write_call_count();

        let resolution = services
            .runtime
            .record_cancel_order_success(
                "btc-core",
                &cancel_effect.effect_id,
                "closed-order",
                &OrderReceipt {
                    order_id: "closed-order".to_string(),
                    client_order_id: blocked_client_order_id.clone(),
                    filled_qty: PositionQty::new(0.05).unwrap(),
                    status: crate::exchange::OrderStatus::Canceled,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            resolution,
            CancelReceiptResolution::ClosedWithFill {
                filled_qty: PositionQty::new(0.05).unwrap(),
            },
            "cancel with fill should let the session queue retire downstream submits"
        );

        let persisted = repository.pending_effects();
        let persisted_cancel = persisted
            .iter()
            .find(|effect| effect.effect_id == cancel_effect.effect_id)
            .expect("cancel effect should remain persisted");
        assert_eq!(persisted_cancel.status, EffectStatus::Succeeded);
        assert_eq!(
            repository.effect_outcome_write_call_count(),
            outcome_writes_before_cancel + 1,
            "cancel-with-fill should record one diagnostic outcome batch for the cancel receipt"
        );

        let snapshot = manager
            .read()
            .await
            .get_track("btc-core")
            .cloned()
            .expect("track runtime");
        let (_, blocked_binding_status) = snapshot
            .binding_receipt_for_client_order_id(&blocked_client_order_id)
            .expect("blocked binding should remain in runtime history");
        assert_eq!(blocked_binding_status, BindingStatus::Terminal);
        assert!(
            !snapshot.has_active_binding_for_order_id(closed_order_id),
            "closed order should no longer have an active runtime binding"
        );
    }
}
