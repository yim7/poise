use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::effect_worker::EffectWorker;
use crate::order_outcome::{ReconcileExecution, ReconcileRequest};
use crate::server_context::{EffectWorkerState, ReconcileState, RuntimeState};
#[cfg(test)]
use crate::test_support::RuntimeTestContext;
use anyhow::{Result, anyhow};
use poise_application::TrackMutationError;
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, ExchangeOrder, ExecutionPort, MarketDataPort,
    MetadataPort, Position, UserDataEvent,
};
use poise_engine::track::Instrument;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

mod account_refresh;
mod guards;
mod market_data;
mod reconcile;
mod startup_sync;
mod submit_preflight;
mod user_data;

pub use guards::{AccountMarginGuardStore, TrackReconcileGuards};
pub(crate) use reconcile::{RecoveryAnomalyDirtyObserver, RecoveryDirtyState};

#[derive(Clone)]
pub struct ServerRuntime {
    state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    account_refresh_interval: Duration,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub(crate) struct RuntimePorts {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}

impl RuntimePorts {
    pub(crate) fn new(
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
    ) -> Self {
        Self {
            execution,
            market_data,
            account,
            metadata,
        }
    }
}

#[derive(Clone, Copy)]
struct RuntimeIntervals {
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    account_refresh_interval: Duration,
}

impl RuntimeIntervals {
    fn new(
        recovery_retry_interval: Duration,
        audit_interval: Duration,
        account_refresh_interval: Duration,
    ) -> Self {
        Self {
            recovery_retry_interval,
            audit_interval,
            account_refresh_interval,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct RuntimeHandles {
    #[cfg_attr(not(test), allow(dead_code))]
    pub market_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub user_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub effect_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub recovery_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub submit_preflight_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub account_task: JoinHandle<()>,
}

const STARTUP_RETRY_ATTEMPTS: usize = 5;
#[cfg(test)]
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(1);

impl ServerRuntime {
    #[cfg(test)]
    pub fn new(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            HashMap::new(),
            RuntimeIntervals::new(
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(5),
            ),
        )
    }

    pub(crate) fn with_account_capacity_snapshots(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        account_capacity_snapshots: HashMap<Instrument, AccountCapacitySnapshot>,
        recovery_retry_interval: Duration,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            account_capacity_snapshots,
            RuntimeIntervals::new(
                recovery_retry_interval,
                Duration::from_secs(5),
                Duration::from_secs(5),
            ),
        )
    }

    #[cfg(test)]
    fn with_reconcile_intervals(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            HashMap::new(),
            RuntimeIntervals::new(
                recovery_retry_interval,
                audit_interval,
                Duration::from_secs(5),
            ),
        )
    }

    #[cfg(test)]
    fn with_reconcile_and_account_refresh_intervals(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
        account_refresh_interval: Duration,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            HashMap::new(),
            RuntimeIntervals::new(
                recovery_retry_interval,
                audit_interval,
                account_refresh_interval,
            ),
        )
    }

    fn with_runtime_options(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        account_capacity_snapshots: HashMap<Instrument, AccountCapacitySnapshot>,
        intervals: RuntimeIntervals,
    ) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        state
            .account_margin_guard
            .replace_snapshots(account_capacity_snapshots);
        Self {
            state,
            effect_worker_state,
            execution: ports.execution,
            market_data: ports.market_data,
            account: ports.account,
            metadata: ports.metadata,
            recovery_retry_interval: intervals.recovery_retry_interval,
            audit_interval: intervals.audit_interval,
            account_refresh_interval: intervals.account_refresh_interval,
            shutdown_tx,
        }
    }

    pub async fn start(&self) -> Result<RuntimeHandles> {
        let mut user_receiver = self.account.subscribe_user_data().await?;
        let startup_cutoff =
            retry_startup_step("get_server_time", || self.metadata.get_server_time()).await?;
        retry_startup_step("startup_sync", || self.startup_sync()).await?;
        self.replay_startup_user_data(&mut user_receiver, startup_cutoff)
            .await?;
        let startup_pending_submit_effects = self
            .state
            .reconcile
            .effect_store
            .list_all_pending_submit_effects()
            .await?;
        self.state
            .reconcile
            .submit_preflight
            .seed_startup_pending_submit_effects(
                startup_pending_submit_effects
                    .into_iter()
                    .map(|effect| effect.effect_id),
            )
            .await;
        let account_task = self.spawn_account_task(self.shutdown_tx.subscribe());
        let recovery_task = self.spawn_recovery_task(self.shutdown_tx.subscribe());
        let submit_preflight_task = self.spawn_submit_preflight_task(self.shutdown_tx.subscribe());
        let effect_task = self.spawn_effect_task(self.shutdown_tx.subscribe());
        let user_task =
            self.spawn_user_task(user_receiver, startup_cutoff, self.shutdown_tx.subscribe());
        let market_task = self.spawn_market_task(self.shutdown_tx.subscribe());

        Ok(RuntimeHandles {
            market_task,
            user_task,
            effect_task,
            recovery_task,
            submit_preflight_task,
            account_task,
        })
    }

    pub async fn shutdown(&self, mut handles: RuntimeHandles) {
        let _ = self.shutdown_tx.send(true);
        tracing::info!("shutdown signal sent");

        let drain_timeout = Duration::from_secs(30);
        if tokio::time::timeout(drain_timeout, &mut handles.effect_task)
            .await
            .is_err()
        {
            tracing::warn!("effect worker drain timed out after {drain_timeout:?}");
            handles.effect_task.abort();
            let _ = handles.effect_task.await;
        }

        let tracks = self
            .state
            .reconcile
            .observation_service
            .track_instruments()
            .await;
        for track in &tracks {
            if let Err(error) = self.execution.cancel_all(&track.instrument).await {
                tracing::warn!(
                    "failed to cancel all orders for {} during shutdown: {error}",
                    track.instrument.symbol
                );
                continue;
            }

            if let Err(error) = sync_exchange_state_from_exchange(
                &self.state.reconcile,
                self.execution.as_ref(),
                &track.id,
                &track.instrument,
                ExchangeSyncMode::RecoverOnly,
            )
            .await
            {
                tracing::warn!(
                    "failed to persist final exchange state for {} during shutdown: {}",
                    track.instrument.symbol,
                    error.message()
                );
            }
        }

        handles.market_task.abort();
        handles.user_task.abort();
        handles.recovery_task.abort();
        handles.submit_preflight_task.abort();
        handles.account_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.recovery_task.await;
        let _ = handles.submit_preflight_task.await;
        let _ = handles.account_task.await;

        tracing::info!("shutdown complete");
    }

    async fn startup_sync(&self) -> Result<()> {
        startup_sync::startup_sync(self).await
    }

    async fn replay_startup_user_data(
        &self,
        receiver: &mut mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        startup_sync::replay_startup_user_data(self, receiver, startup_cutoff).await
    }

    fn spawn_market_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        market_data::spawn_market_task(self, shutdown_rx)
    }

    fn spawn_effect_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        EffectWorker::with_shutdown_rx(
            self.effect_worker_state.clone(),
            Arc::clone(&self.execution),
            Arc::clone(&self.account),
            Duration::from_millis(10),
            shutdown_rx,
        )
        .spawn()
    }

    fn spawn_recovery_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        reconcile::spawn_recovery_task(self, shutdown_rx)
    }

    fn spawn_submit_preflight_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        submit_preflight::spawn_submit_preflight_task(self, shutdown_rx)
    }

    fn spawn_account_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        account_refresh::spawn_account_task(self, shutdown_rx)
    }

    fn spawn_user_task(
        &self,
        receiver: mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        user_data::spawn_user_task(self, receiver, startup_cutoff, shutdown_rx)
    }
}

pub(crate) trait ReconcileStateAccess {
    fn reconcile_state_view(&self) -> ReconcileState;
}

impl ReconcileStateAccess for ReconcileState {
    fn reconcile_state_view(&self) -> ReconcileState {
        self.clone()
    }
}

#[cfg(test)]
impl ReconcileStateAccess for RuntimeTestContext {
    fn reconcile_state_view(&self) -> ReconcileState {
        self.runtime_state().reconcile.clone()
    }
}

async fn retry_startup_step<T, F, Fut>(step_name: &'static str, operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    startup_sync::retry_startup_step(step_name, operation).await
}

async fn apply_user_data_event(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    track_id: &str,
    event: UserDataEvent,
) -> std::result::Result<(), TrackMutationError> {
    let state = state.reconcile_state_view();
    startup_sync::apply_user_data_event(&state, execution, track_id, event).await
}

pub(crate) async fn enqueue_reconcile_request(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    request: ReconcileRequest,
    instrument: &poise_engine::track::Instrument,
) -> std::result::Result<ReconcileExecution, TrackMutationError> {
    reconcile::enqueue_reconcile_request(state, execution, request, instrument).await
}

async fn sync_exchange_state_from_exchange(
    state: &impl ReconcileStateAccess,
    execution: &dyn ExecutionPort,
    track_id: &str,
    instrument: &poise_engine::track::Instrument,
    mode: ExchangeSyncMode,
) -> std::result::Result<(), TrackMutationError> {
    reconcile::sync_exchange_state_from_exchange(state, execution, track_id, instrument, mode).await
}

fn preserve_track_mutation_error(error: anyhow::Error) -> TrackMutationError {
    match error.downcast::<TrackMutationError>() {
        Ok(error) => error,
        Err(other) => TrackMutationError::Persistence(other),
    }
}

fn mutate_error(error: TrackMutationError) -> anyhow::Error {
    anyhow!(error.message())
}

fn position_observation(position: &Position) -> PositionObservation {
    PositionObservation {
        qty: position.qty,
        unrealized_pnl: position.unrealized_pnl,
    }
}

fn order_observation(order: &ExchangeOrder) -> OrderObservation {
    OrderObservation {
        order_id: order.order_id.clone(),
        client_order_id: order.client_order_id.clone(),
        side: order.side,
        price: order.price,
        quantity: order.qty,
        realized_pnl: order.realized_pnl,
        status: order.status,
    }
}

#[cfg(test)]
mod tests;
