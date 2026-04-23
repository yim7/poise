use std::sync::Arc;
use std::time::Duration;

use crate::effect_worker::EffectWorker;
use crate::order_outcome::{ReconcileExecution, ReconcileRequest};
use crate::server_context::{EffectWorkerState, ReconcileState, RuntimeState};
#[cfg(test)]
use crate::test_support::RuntimeTestContext;
use anyhow::{Result, anyhow};
use poise_application::TrackMutationError;
use poise_application::TrackStartupDefinition;
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::ports::{
    AccountPort, AccountSummaryPort, ClockPort, ExecutionPort, MarketDataPort, MetadataPort,
    UserDataEvent,
};
use poise_engine::track::{Instrument, TrackId};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

mod account_refresh;
mod exchange_state;
mod guards;
mod market_data;
mod market_data_health;
mod reconcile;
mod startup_bootstrap;
mod submit_preflight;
mod user_data;

pub use guards::{AccountMarginGuardStore, TrackReconcileGuards};
pub(crate) use reconcile::{RecoveryAnomalyDirtyObserver, RecoveryDirtyState};

#[derive(Clone)]
pub(crate) enum RuntimeStartupCapacityMode {
    AccountCapacitySnapshot,
    AvailableBalanceTimesLeverage { leverage: u32 },
}

#[derive(Clone)]
pub(crate) struct RuntimeStartupDefinition {
    track: TrackStartupDefinition,
    capacity_mode: RuntimeStartupCapacityMode,
}

impl RuntimeStartupDefinition {
    pub(crate) fn new(
        track: TrackStartupDefinition,
        capacity_mode: RuntimeStartupCapacityMode,
    ) -> Self {
        Self {
            track,
            capacity_mode,
        }
    }

    pub(crate) fn track_id(&self) -> &TrackId {
        self.track.track_id()
    }

    pub(crate) fn instrument(&self) -> &Instrument {
        self.track.instrument()
    }

    pub(crate) fn required_additional_notional(&self, position_qty: f64) -> f64 {
        self.track.required_additional_notional(position_qty)
    }

    pub(crate) fn startup_capacity_mode(&self) -> &RuntimeStartupCapacityMode {
        &self.capacity_mode
    }
}

#[derive(Clone)]
pub struct ServerRuntime {
    state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account: Arc<dyn AccountPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    metadata: Arc<dyn MetadataPort>,
    clock: Arc<dyn ClockPort>,
    startup_definitions: Vec<RuntimeStartupDefinition>,
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    account_refresh_interval: Duration,
    market_data_health_state: Arc<market_data_health::MarketDataHealthState>,
    market_data_health_max_sleep_interval: Duration,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub(crate) struct RuntimePorts {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account: Arc<dyn AccountPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    metadata: Arc<dyn MetadataPort>,
    clock: Arc<dyn ClockPort>,
}

impl RuntimePorts {
    pub(crate) fn new(
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account_summary: Arc<dyn AccountSummaryPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
        clock: Arc<dyn ClockPort>,
    ) -> Self {
        Self {
            execution,
            market_data,
            account,
            account_summary,
            metadata,
            clock,
        }
    }
}

#[derive(Clone, Copy)]
struct RuntimeIntervals {
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    account_refresh_interval: Duration,
    market_data_health_max_sleep_interval: Duration,
}

impl RuntimeIntervals {
    fn new(
        recovery_retry_interval: Duration,
        audit_interval: Duration,
        account_refresh_interval: Duration,
        market_data_health_max_sleep_interval: Duration,
    ) -> Self {
        Self {
            recovery_retry_interval,
            audit_interval,
            account_refresh_interval,
            market_data_health_max_sleep_interval,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub struct RuntimeHandles {
    #[cfg_attr(not(test), allow(dead_code))]
    pub market_task: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub market_data_health_task: JoinHandle<()>,
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
        startup_definitions: Vec<RuntimeStartupDefinition>,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            startup_definitions,
            RuntimeIntervals::new(
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_millis(50),
            ),
        )
    }

    pub(crate) fn with_startup_definitions(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        startup_definitions: Vec<RuntimeStartupDefinition>,
        recovery_retry_interval: Duration,
    ) -> Self {
        Self::with_runtime_options(
            state,
            effect_worker_state,
            ports,
            startup_definitions,
            RuntimeIntervals::new(
                recovery_retry_interval,
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(1),
            ),
        )
    }

    fn with_runtime_options(
        state: RuntimeState,
        effect_worker_state: EffectWorkerState,
        ports: RuntimePorts,
        startup_definitions: Vec<RuntimeStartupDefinition>,
        intervals: RuntimeIntervals,
    ) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            state,
            effect_worker_state,
            execution: ports.execution,
            market_data: ports.market_data,
            account: ports.account,
            account_summary: ports.account_summary,
            metadata: ports.metadata,
            clock: ports.clock,
            startup_definitions,
            recovery_retry_interval: intervals.recovery_retry_interval,
            audit_interval: intervals.audit_interval,
            account_refresh_interval: intervals.account_refresh_interval,
            market_data_health_state: Arc::new(market_data_health::MarketDataHealthState::default()),
            market_data_health_max_sleep_interval: intervals.market_data_health_max_sleep_interval,
            shutdown_tx,
        }
    }

    pub async fn start(&self) -> Result<RuntimeHandles> {
        let mut user_receiver = self.account.subscribe_user_data().await?;
        let startup_cutoff = startup_bootstrap::retry_startup_step("get_server_time", || {
            self.metadata.get_server_time()
        })
        .await?;
        let steady_state_cutoff =
            startup_bootstrap::complete_startup(self, &mut user_receiver, startup_cutoff).await?;
        let account_task = self.spawn_account_task(self.shutdown_tx.subscribe());
        let recovery_task = self.spawn_recovery_task(self.shutdown_tx.subscribe());
        let market_data_health_task =
            self.spawn_market_data_health_task(self.shutdown_tx.subscribe());
        let submit_preflight_task = self.spawn_submit_preflight_task(self.shutdown_tx.subscribe());
        let effect_task = self.spawn_effect_task(self.shutdown_tx.subscribe());
        let user_task =
            self.spawn_user_task(user_receiver, steady_state_cutoff, self.shutdown_tx.subscribe());
        let market_task = self.spawn_market_task(self.shutdown_tx.subscribe());

        Ok(RuntimeHandles {
            market_task,
            market_data_health_task,
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
        handles.market_data_health_task.abort();
        handles.user_task.abort();
        handles.recovery_task.abort();
        handles.submit_preflight_task.abort();
        handles.account_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.market_data_health_task.await;
        let _ = handles.user_task.await;
        let _ = handles.recovery_task.await;
        let _ = handles.submit_preflight_task.await;
        let _ = handles.account_task.await;

        tracing::info!("shutdown complete");
    }

    fn spawn_market_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        market_data::spawn_market_task(self, shutdown_rx)
    }

    fn spawn_market_data_health_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        market_data_health::spawn_market_data_health_task(self, shutdown_rx)
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
