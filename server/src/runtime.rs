use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use poise_engine::manager::ExchangeSyncMode;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::ports::{
    AccountMarginSnapshot, ExchangeOrder, ExchangePort, MarketDataPort, Position, UserDataEvent,
    UserDataPayload,
};
use poise_engine::runtime::AccountCapacityConstraint;
use poise_engine::track::{Instrument, TrackId};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior, sleep};

use crate::assembly::ServerState;
use crate::effect_worker::EffectWorker;
use crate::notifications::TrackInternalNotification;
use crate::order_outcome::{
    ReconcileExecution, ReconcileReason, ReconcileRequest, reconcile_execution,
};
use crate::write_service::TrackMutationError;

#[derive(Clone)]
pub struct ServerRuntime {
    state: ServerState,
    exchange: Arc<dyn ExchangePort>,
    market_data: Arc<dyn MarketDataPort>,
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct AccountMarginGuard {
    pub snapshot: Option<AccountMarginSnapshot>,
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub blocked_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub(crate) struct AccountMarginGuardStore {
    inner: std::sync::Mutex<HashMap<Instrument, AccountMarginGuard>>,
}

impl AccountMarginGuardStore {
    pub(crate) fn replace_snapshots(&self, snapshots: HashMap<Instrument, AccountMarginSnapshot>) {
        let mut guards = self.inner.lock().unwrap();
        for (instrument, snapshot) in snapshots {
            guards.entry(instrument).or_default().snapshot = Some(snapshot);
        }
    }

    pub(crate) fn update_snapshot(&self, instrument: Instrument, snapshot: AccountMarginSnapshot) {
        self.inner.lock().unwrap().entry(instrument).or_default().snapshot = Some(snapshot);
    }

    pub(crate) fn activate_insufficient_margin(
        &self,
        instrument: &Instrument,
        reason: impl Into<String>,
        blocked_at: DateTime<Utc>,
    ) {
        let reason = reason.into();
        let mut guards = self.inner.lock().unwrap();
        let mut matched = false;
        for (tracked_instrument, guard) in guards.iter_mut() {
            if tracked_instrument.venue != instrument.venue {
                continue;
            }
            guard.increase_blocked = true;
            guard.blocked_reason = Some(reason.clone());
            guard.blocked_at = Some(blocked_at);
            matched = true;
        }

        if !matched {
            guards.insert(
                instrument.clone(),
                AccountMarginGuard {
                    snapshot: None,
                    increase_blocked: true,
                    blocked_reason: Some(reason),
                    blocked_at: Some(blocked_at),
                },
            );
        }
    }

    pub(crate) fn constraint_for(&self, instrument: &Instrument) -> AccountCapacityConstraint {
        self.inner
            .lock()
            .unwrap()
            .get(instrument)
            .map(|guard| AccountCapacityConstraint {
                increase_blocked: guard.increase_blocked,
                blocked_reason: guard.blocked_reason.clone(),
                max_increase_notional: guard.snapshot.as_ref().map(|snapshot| snapshot.max_increase_notional),
            })
            .unwrap_or_default()
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
}

const STARTUP_RETRY_ATTEMPTS: usize = 5;
#[cfg(test)]
const STARTUP_RETRY_DELAY: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct RecoveryTrackedGrid {
    instrument: poise_engine::track::Instrument,
    next_retry_at: Instant,
}

impl ServerRuntime {
    pub fn new(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        market_data: Arc<dyn MarketDataPort>,
    ) -> Self {
        Self::with_reconcile_intervals_and_account_margin_snapshots(
            state,
            exchange,
            market_data,
            HashMap::new(),
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
    }

    pub(crate) fn with_account_margin_snapshots(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        market_data: Arc<dyn MarketDataPort>,
        account_margin_snapshots: HashMap<Instrument, AccountMarginSnapshot>,
        recovery_retry_interval: Duration,
    ) -> Self {
        Self::with_reconcile_intervals_and_account_margin_snapshots(
            state,
            exchange,
            market_data,
            account_margin_snapshots,
            recovery_retry_interval,
            Duration::from_secs(5),
        )
    }

    fn with_reconcile_intervals(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        market_data: Arc<dyn MarketDataPort>,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
    ) -> Self {
        Self::with_reconcile_intervals_and_account_margin_snapshots(
            state,
            exchange,
            market_data,
            HashMap::new(),
            recovery_retry_interval,
            audit_interval,
        )
    }

    fn with_reconcile_intervals_and_account_margin_snapshots(
        state: ServerState,
        exchange: Arc<dyn ExchangePort>,
        market_data: Arc<dyn MarketDataPort>,
        account_margin_snapshots: HashMap<Instrument, AccountMarginSnapshot>,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
    ) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        state
            .account_margin_guard
            .replace_snapshots(account_margin_snapshots);
        Self {
            state,
            exchange,
            market_data,
            recovery_retry_interval,
            audit_interval,
            shutdown_tx,
        }
    }

    pub async fn start(&self) -> Result<RuntimeHandles> {
        let mut user_receiver = self.market_data.subscribe_user_data().await?;
        let startup_cutoff =
            retry_startup_step("get_server_time", || self.exchange.get_server_time()).await?;
        retry_startup_step("startup_sync", || self.startup_sync()).await?;
        self.replay_startup_user_data(&mut user_receiver, startup_cutoff)
            .await?;
        let recovery_task = self.spawn_recovery_task(self.shutdown_tx.subscribe());
        let effect_task = self.spawn_effect_task(self.shutdown_tx.subscribe());
        let user_task =
            self.spawn_user_task(user_receiver, startup_cutoff, self.shutdown_tx.subscribe());
        let market_task = self.spawn_market_task(self.shutdown_tx.subscribe());

        Ok(RuntimeHandles {
            market_task,
            user_task,
            effect_task,
            recovery_task,
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

        let tracks = self.state.write_service.track_instruments().await;
        for track in &tracks {
            if let Err(error) = self.exchange.cancel_all(&track.instrument).await {
                tracing::warn!(
                    "failed to cancel all orders for {} during shutdown: {error}",
                    track.instrument.symbol
                );
                continue;
            }

            if let Err(error) = sync_exchange_state_from_exchange(
                &self.state,
                &self.exchange,
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
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.recovery_task.await;

        tracing::info!("shutdown complete");
    }

    async fn startup_sync(&self) -> Result<()> {
        for track in self.state.write_service.track_instruments().await {
            let position = self.exchange.get_position(&track.instrument).await?;
            let open_orders = self.exchange.get_open_orders(&track.instrument).await?;
            self.state
                .write_service
                .sync_exchange_state(
                    &track.id,
                    position_observation(&position),
                    open_orders.iter().map(order_observation).collect(),
                )
                .await?;
        }

        Ok(())
    }

    async fn replay_startup_user_data(
        &self,
        receiver: &mut mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut buffered_events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            buffered_events.push(event);
        }

        buffered_events.sort_by_key(|event| event.event_time);
        for event in buffered_events {
            if event.event_time > startup_cutoff {
                let instrument = event.instrument().clone();
                let Some(track_id) = self.state.write_service.resolve_track_id(&instrument).await
                else {
                    tracing::warn!(
                        "received user data for unknown instrument {}:{}",
                        instrument.venue.as_str(),
                        instrument.symbol
                    );
                    continue;
                };
                apply_user_data_event(&self.state, &self.exchange, &track_id, event)
                    .await
                    .map_err(mutate_error)?;
            }
        }

        Ok(())
    }

    fn spawn_market_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        let state = self.state.clone();
        let market_data = Arc::clone(&self.market_data);

        tokio::spawn(async move {
            let tracks = state.write_service.track_instruments().await;
            let mut workers = JoinSet::new();

            for track in tracks {
                if *shutdown_rx.borrow() {
                    break;
                }

                let instrument = track.instrument.clone();
                match market_data.subscribe_prices(&instrument).await {
                    Ok(mut receiver) => {
                        let state = state.clone();
                        let mut worker_shutdown_rx = shutdown_rx.clone();
                        workers.spawn(async move {
                            loop {
                                if *worker_shutdown_rx.borrow() {
                                    break;
                                }

                                tokio::select! {
                                    biased;
                                    changed = worker_shutdown_rx.changed() => {
                                        if changed.is_err() || *worker_shutdown_rx.borrow() {
                                            break;
                                        }
                                    }
                                    tick = receiver.recv() => {
                                        let Some(tick) = tick else {
                                            break;
                                        };

                                        match state
                                            .write_service
                                            .observe_market(&track.id, tick.reference_price)
                                            .await
                                        {
                                            Ok(_) => {}
                                            Err(error) => {
                                                tracing::warn!(
                                                    "failed to apply market data update for {}: {}",
                                                    instrument.symbol,
                                                    error
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(
                            "failed to subscribe market data for {}: {error}",
                            instrument.symbol
                        );
                    }
                }
            }

            while let Some(result) = workers.join_next().await {
                if let Err(error) = result {
                    tracing::warn!("market worker join error: {error}");
                }
            }
        })
    }

    fn spawn_effect_task(&self, shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        EffectWorker::with_shutdown_rx(
            self.state.clone(),
            Arc::clone(&self.exchange),
            Duration::from_millis(10),
            shutdown_rx,
        )
        .spawn()
    }

    fn spawn_recovery_task(&self, mut shutdown_rx: watch::Receiver<bool>) -> JoinHandle<()> {
        let state = self.state.clone();
        let exchange = Arc::clone(&self.exchange);
        let retry_interval = self.recovery_retry_interval;
        let audit_interval = self.audit_interval;

        tokio::spawn(async move {
            let instruments = state.write_service.track_instruments().await;
            let mut tracked = seed_recovery_tracking(&state, &instruments, retry_interval).await;
            let mut next_audit_at = instruments
                .iter()
                .map(|track| (track.id.clone(), Instant::now() + audit_interval))
                .collect::<std::collections::HashMap<_, _>>();
            let mut notifications = state.write_service.subscribe_notifications();
            let mut ticker = tokio::time::interval(Duration::from_millis(50));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        for track in &instruments {
                            if let Err(error) = state
                                .write_service
                                .refresh_market_data_health(&track.id)
                                .await
                            {
                                tracing::warn!(
                                    "failed to refresh market data health for {}: {}",
                                    track.instrument.symbol,
                                    error
                                );
                            }
                        }

                        let now = Instant::now();
                        let due_anomaly_tracks: Vec<(String, poise_engine::track::Instrument)> = tracked
                            .iter()
                            .filter(|(_, tracked_track)| tracked_track.next_retry_at <= now)
                            .map(|(track_id, tracked_track)| (track_id.clone(), tracked_track.instrument.clone()))
                            .collect();
                        let due_audit_tracks: Vec<(String, poise_engine::track::Instrument)> = instruments
                            .iter()
                            .filter(|track| {
                                next_audit_at
                                    .get(&track.id)
                                    .is_some_and(|next_audit| *next_audit <= now)
                            })
                            .map(|track| (track.id.clone(), track.instrument.clone()))
                            .collect();

                        let mut due_tracks = due_audit_tracks
                            .into_iter()
                            .collect::<std::collections::HashMap<_, _>>();
                        for (track_id, instrument) in due_anomaly_tracks {
                            due_tracks.insert(track_id, instrument);
                        }

                        for (track_id, instrument) in due_tracks {
                            if let Some(tracked_track) = tracked.get_mut(&track_id) {
                                tracked_track.next_retry_at = Instant::now() + retry_interval;
                            }
                            next_audit_at.insert(track_id.clone(), Instant::now() + audit_interval);
                            if let Err(error) = sync_exchange_state_from_exchange(
                                &state,
                                &exchange,
                                &track_id,
                                &instrument,
                                ExchangeSyncMode::RecoverAndReconcile,
                            )
                            .await {
                                tracing::warn!(
                                    "failed to auto-resync recovery anomaly for {}: {}",
                                    instrument.symbol,
                                    error.message()
                                );
                            }
                        }
                    }
                    notification = notifications.recv() => {
                        match notification {
                            Ok(TrackInternalNotification::TrackWriteCommitted {
                                track_id,
                                recovery_anomaly_active,
                            }) => {
                                update_recovery_tracking(
                                    &mut tracked,
                                    &instruments,
                                    track_id.as_str(),
                                    recovery_anomaly_active,
                                    retry_interval,
                                );
                            }
                            Ok(TrackInternalNotification::TrackEffectStateChanged { .. }) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                tracing::warn!(
                                    "recovery notification stream lagged by {skipped} messages; reseeding recovery tracking"
                                );
                                tracked = seed_recovery_tracking(&state, &instruments, retry_interval).await;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        })
    }

    fn spawn_user_task(
        &self,
        mut receiver: mpsc::Receiver<UserDataEvent>,
        startup_cutoff: chrono::DateTime<chrono::Utc>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let state = self.state.clone();
        let exchange = Arc::clone(&self.exchange);

        tokio::spawn(async move {
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                let event = tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                    event = receiver.recv() => event,
                };

                let Some(event) = event else {
                    break;
                };

                if event.event_time <= startup_cutoff {
                    continue;
                }

                let instrument = event.instrument().clone();
                let Some(track_id) = state.write_service.resolve_track_id(&instrument).await else {
                    tracing::warn!(
                        "received user data for unknown instrument {}:{}",
                        instrument.venue.as_str(),
                        instrument.symbol
                    );
                    continue;
                };
                if let Err(error) =
                    apply_user_data_event(&state, &exchange, &track_id, event).await
                {
                    tracing::warn!(
                        "failed to apply user data update for {}: {}",
                        instrument.symbol,
                        error.message()
                    );
                    continue;
                }
            }
        })
    }
}

async fn retry_startup_step<T, F, Fut>(step_name: &'static str, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut last_error = None;

    for attempt in 0..STARTUP_RETRY_ATTEMPTS {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 == STARTUP_RETRY_ATTEMPTS {
                    return Err(error);
                }
                tracing::warn!(
                    step = step_name,
                    attempt = attempt + 1,
                    max_attempts = STARTUP_RETRY_ATTEMPTS,
                    "startup step failed: {error}"
                );
                last_error = Some(error);
            }
        }

        sleep(STARTUP_RETRY_DELAY).await;
    }

    Err(last_error.unwrap_or_else(|| anyhow!("startup step `{step_name}` failed")))
}

async fn apply_user_data_event(
    state: &ServerState,
    exchange: &Arc<dyn ExchangePort>,
    track_id: &str,
    event: UserDataEvent,
) -> std::result::Result<(), TrackMutationError> {
    let instrument = event.instrument().clone();
    match event.payload {
        UserDataPayload::PositionUpdate(position) => {
            let _ = state
                .write_service
                .observe_position(track_id, position_observation(&position))
                .await
                .map_err(preserve_track_mutation_error)?;
        }
        UserDataPayload::OrderUpdate(order) => {
            let (_, absorb_result): (_, poise_engine::executor::OrderUpdateAbsorbResult) = state
                .write_service
                .observe_order_with_absorb_result(track_id, order_observation(&order))
                .await
                .map_err(preserve_track_mutation_error)?;
            if absorb_result == poise_engine::executor::OrderUpdateAbsorbResult::Unabsorbed {
                enqueue_reconcile_request(
                    state,
                    exchange,
                    ReconcileRequest {
                        track_id: track_id.to_string(),
                        reason: ReconcileReason::UnabsorbedOrderUpdate,
                    },
                    &instrument,
                )
                .await?;
            }
        }
    }

    Ok(())
}

pub(crate) async fn enqueue_reconcile_request(
    state: &ServerState,
    exchange: &Arc<dyn ExchangePort>,
    request: ReconcileRequest,
    instrument: &poise_engine::track::Instrument,
) -> std::result::Result<ReconcileExecution, TrackMutationError> {
    let execution = reconcile_execution(&request.track_id, vec![request.reason]);
    sync_exchange_state_from_exchange(
        state,
        exchange,
        &request.track_id,
        instrument,
        ExchangeSyncMode::RecoverAndReconcile,
    )
    .await?;
    Ok(execution)
}

async fn sync_exchange_state_from_exchange(
    state: &ServerState,
    exchange: &Arc<dyn ExchangePort>,
    track_id: &str,
    instrument: &poise_engine::track::Instrument,
    mode: ExchangeSyncMode,
) -> std::result::Result<(), TrackMutationError> {
    let snapshot = state
        .state_repository
        .load_track_state(track_id)
        .await
        .map_err(TrackMutationError::Persistence)?;
    let mut position = exchange
        .get_position(instrument)
        .await
        .map_err(TrackMutationError::Persistence)?;
    let mut open_orders = exchange
        .get_open_orders(instrument)
        .await
        .map_err(TrackMutationError::Persistence)?;

    if should_cancel_unknown_live_orders(snapshot.as_ref(), &open_orders)
        && pending_submit_hints_are_empty(state, track_id).await?
    {
        for order in &open_orders {
            exchange
                .cancel_order(instrument, &order.order_id)
                .await
                .with_context(|| {
                    format!(
                        "failed to cancel unknown live order `{}` for {}",
                        order.order_id, instrument.symbol
                    )
                })
                .map_err(TrackMutationError::Persistence)?;
        }
        position = exchange
            .get_position(instrument)
            .await
            .map_err(TrackMutationError::Persistence)?;
        open_orders = exchange
            .get_open_orders(instrument)
            .await
            .map_err(TrackMutationError::Persistence)?;
    }

    if matches!(mode, ExchangeSyncMode::RecoverAndReconcile) {
        let _ = state
            .write_service
            .sync_exchange_state(
                track_id,
                position_observation(&position),
                open_orders.iter().map(order_observation).collect(),
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    } else {
        let _ = state
            .write_service
            .sync_exchange_state_without_reconcile(
                track_id,
                position_observation(&position),
                open_orders.iter().map(order_observation).collect(),
            )
            .await
            .map_err(preserve_track_mutation_error)?;
    }
    Ok(())
}

fn should_cancel_unknown_live_orders(
    snapshot: Option<&poise_engine::snapshot::TrackRuntimeSnapshot>,
    open_orders: &[ExchangeOrder],
) -> bool {
    !open_orders.is_empty()
        && snapshot.is_some_and(|snapshot| {
            snapshot.executor_state.recovery_anomaly
                == Some(poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
                && snapshot
                    .executor_state
                    .slots
                    .iter()
                    .all(|slot| slot.working_order.is_none())
        })
}

async fn pending_submit_hints_are_empty(
    state: &ServerState,
    track_id: &str,
) -> std::result::Result<bool, TrackMutationError> {
    let pending_submit_hints = state
        .state_repository
        .list_pending_submit_effects_for_track(&TrackId::new(track_id))
        .await
        .map_err(TrackMutationError::Persistence)?;
    Ok(pending_submit_hints.is_empty())
}

fn update_recovery_tracking(
    tracked: &mut std::collections::HashMap<String, RecoveryTrackedGrid>,
    instruments: &[crate::write_service::TrackInstrument],
    track_id: &str,
    recovery_anomaly_active: bool,
    retry_interval: Duration,
) {
    if !recovery_anomaly_active {
        tracked.remove(track_id);
        return;
    }

    let Some(instrument) = instruments
        .iter()
        .find(|track| track.id == track_id)
        .map(|track| track.instrument.clone())
    else {
        return;
    };

    tracked
        .entry(track_id.to_string())
        .or_insert_with(|| RecoveryTrackedGrid {
            instrument,
            next_retry_at: Instant::now() + retry_interval,
        });
}

async fn seed_recovery_tracking(
    state: &ServerState,
    instruments: &[crate::write_service::TrackInstrument],
    retry_interval: Duration,
) -> std::collections::HashMap<String, RecoveryTrackedGrid> {
    let mut tracked = std::collections::HashMap::new();
    for track in instruments {
        let Ok(Some(snapshot)) = state.state_repository.load_track_state(&track.id).await else {
            continue;
        };
        update_recovery_tracking(
            &mut tracked,
            instruments,
            &track.id,
            snapshot.executor_state.recovery_anomaly.is_some(),
            retry_interval,
        );
    }
    tracked
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
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::io;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use chrono::{TimeZone, Utc};
    use poise_core::events::DomainEvent;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Exposure, Side};
    use poise_engine::command::TrackCommand;
    use poise_engine::execution_plan::ExecutionAction;
    use poise_engine::executor::{ExecutionMode, OrderRole, OrderSlot};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        ClockPort, CommittedTrackWrite, EffectStatus, EffectStatusUpdate, ExchangeInfo,
        ExchangeOrder, ExchangePort, MarketDataPort, OrderReceipt, OrderRequest, OrderStatus,
        PersistedTrackEffect, Position, PriceTick, StateRepositoryPort, StoredTrackEvent,
        StoredTrackSnapshot, TrackReadRepositoryPort, TrackSnapshot, UserDataEvent,
        UserDataPayload,
    };
    use poise_engine::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus,
        WorkingOrder,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
    use tokio::time::{sleep, timeout};
    use tracing_subscriber::fmt::MakeWriter;

    use crate::assembly::{ServerState, build_server_state};
    use crate::effect_worker::EffectWorker;
    use crate::projector::TrackProjector;
    use crate::query_service::TrackQueryService;
    use crate::write_service::TrackWriteService;

    use super::{RuntimeHandles, ServerRuntime};

    #[tokio::test]
    async fn market_tick_submits_order_and_records_inventory_core_slot() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_instance(&fixture.state, |instance| {
            inventory_core_order(instance).is_some()
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        let order = inventory_core_order(&instance).unwrap();
        assert_eq!(order.order_id.as_deref(), Some("order-1"));
        assert_eq!(order.target_exposure, Exposure(4.0));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn start_retries_transient_startup_failures() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
        fixture.exchange.fail_next_server_time_requests(2);
        fixture.exchange.fail_next_open_orders_requests(1);

        let handles = fixture.runtime.start().await.unwrap();

        assert_eq!(
            fixture
                .exchange
                .get_server_time_calls
                .load(Ordering::SeqCst),
            3
        );
        assert_eq!(
            fixture.exchange.get_position_calls.load(Ordering::SeqCst),
            2
        );
        assert_eq!(
            fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst),
            2
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_executes_persisted_submit_order_and_marks_success() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let transition = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(
            fixture
                .persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        let handles = fixture.runtime.start().await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_async(|| {
            let persistence = Arc::clone(&fixture.persistence);
            async move {
                persistence
                    .list_dispatchable_effects()
                    .await
                    .unwrap()
                    .is_empty()
            }
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(
            inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
            Some("order-1")
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn repeated_ticks_before_first_submit_are_absorbed_into_one_replacement_plan() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let first = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            first.effects.as_slice(),
            [ExecutionAction::SubmitOrder { .. }]
        ));

        let second = state
            .write_service
            .observe_market("BTCUSDT", 92.5)
            .await
            .unwrap();
        assert_eq!(
            second.effects,
            vec![ExecutionAction::NoOp],
            "new tick should update target only while first submit intent is pending"
        );

        worker.run_once().await.unwrap();

        let submitted = exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted.len(), 1);
        assert!(matches!(
            submitted.as_slice(),
            [OrderRequest {
                side: Side::Buy,
                price,
                quantity,
                ..
            }] if (*price - 92.5).abs() < f64::EPSILON
                && (*quantity - test_config().base_qty_per_unit() * 6.0).abs() < f64::EPSILON
        ));
        assert!(
            persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty(),
            "replacement submit should not leave duplicate pending submit effects behind"
        );
    }

    #[tokio::test]
    async fn effect_worker_restores_pending_effect_after_restart() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .len(),
            1
        );

        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let restarted_runtime = ServerRuntime::new(
            fixture.state.clone(),
            fixture.exchange.clone() as Arc<dyn ExchangePort>,
            Arc::new(FakeMarketData::new(price_receiver, user_receiver)) as Arc<dyn MarketDataPort>,
        );

        let handles = restarted_runtime.start().await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_async(|| {
            let persistence = Arc::clone(&fixture.persistence);
            async move {
                persistence
                    .list_dispatchable_effects()
                    .await
                    .unwrap()
                    .is_empty()
            }
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn failed_effect_does_not_roll_back_committed_snapshot() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            "submit rejected",
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        assert_eq!(
            persistence.list_dispatchable_effects().await.unwrap().len(),
            1
        );

        let handles = runtime.start().await.unwrap();

        wait_until_async(|| {
            let persistence = Arc::clone(&persistence);
            async move {
                persistence
                    .all_effects()
                    .await
                    .iter()
                    .any(|effect| effect.status == EffectStatus::Failed)
            }
        })
        .await;

        let instance = current_instance(&state).await;
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert!(inventory_core_order(&instance).is_none());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn insufficient_margin_guard_activates_after_exchange_rejects_submit() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            r#"request POST /fapi/v1/order failed with status 400 Bad Request: {"code":-2019,"msg":"Margin is insufficient."}"#,
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );

        let handles = runtime.start().await.unwrap();

        wait_until_async(|| {
            let persistence = Arc::clone(&persistence);
            async move {
                persistence
                    .list_dispatchable_effects()
                    .await
                    .unwrap()
                    .is_empty()
            }
        })
        .await;

        let constraint = state.account_margin_guard.constraint_for(&btc_instrument());
        assert!(constraint.increase_blocked);
        assert_eq!(
            constraint.blocked_reason.as_deref(),
            Some("insufficient_margin")
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn insufficient_margin_guard_blocks_follow_up_submit_after_market_tick() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            r#"request POST /fapi/v1/order failed with status 400 Bad Request: {"code":-2019,"msg":"Margin is insufficient."}"#,
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let (_price_sender, price_receiver) = mpsc::channel(8);
        let (_user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();

        let handles = runtime.start().await.unwrap();

        wait_until(|| {
            state
                .account_margin_guard
                .constraint_for(&btc_instrument())
                .increase_blocked
        })
        .await;

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();

        assert!(
            transition
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::RiskDenied { .. }))
        );
        assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
        assert_eq!(exchange.submitted_orders.lock().unwrap().len(), 1);

        let instance = current_instance(&state).await;
        assert!(instance.risk.account_capacity_constraint.increase_blocked);
        let source = state
            .query_service
            .load_track_detail_source(&TrackId::new("BTCUSDT"))
            .await
            .unwrap()
            .unwrap();
        let detail = state.projector.project_detail(&source);
        assert_eq!(
            detail.execution.execution_status,
            poise_protocol::ExecutionStatusView::AttentionRequired
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_leaves_submitting_working_order_when_receipt_persistence_fails() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(FailOnReceiptPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );
        worker.run_once().await.unwrap();

        let instance = current_instance(&state).await;
        let order = inventory_core_order(&instance).expect("submit intent should remain durable");
        assert_eq!(order.order_id, None);
        assert_eq!(order.status, OrderStatus::Submitting);

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Failed);
    }

    #[tokio::test]
    async fn effect_worker_skips_stale_submit_when_track_is_paused_before_execution() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let transition = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [ExecutionAction::SubmitOrder { .. }]
        ));

        fixture
            .state
            .write_service
            .command("BTCUSDT", TrackCommand::Pause)
            .await
            .unwrap();
        let handles = fixture.runtime.start().await.unwrap();
        wait_until_async(|| {
            let persistence = fixture.persistence.clone();
            async move {
                persistence.all_effects().await.iter().any(|effect| {
                    effect.status == EffectStatus::Superseded
                        && matches!(effect.effect, ExecutionAction::SubmitOrder { .. })
                })
            }
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.target_exposure, None);
        assert!(inventory_core_order(&instance).is_none());
        assert!(
            fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
            "paused track should not execute stale submit effects"
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_skips_stale_submit_when_current_exposure_has_changed() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(2.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot.clone()),
            test_budget(),
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:stale:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "stale".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: test_config().base_qty_per_unit() * 4.0,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(4.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        worker.run_once().await.unwrap();

        let submitted = exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(
            submitted.len(),
            1,
            "replacement submit should run in the same worker iteration"
        );
        assert!(matches!(
            submitted.as_slice(),
            [OrderRequest {
                side: Side::Buy,
                price,
                quantity,
                ..
            }] if (*price - 95.0).abs() < f64::EPSILON
                && (*quantity - test_config().base_qty_per_unit() * 2.0).abs() < f64::EPSILON
        ));
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects
                .iter()
                .find(|effect| effect.effect_id == "BTCUSDT:stale:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
        let replacement = effects
            .iter()
            .find(|effect| effect.effect_id != "BTCUSDT:stale:0")
            .expect("replacement submit should be persisted for the current target");
        assert_eq!(replacement.status, EffectStatus::Succeeded);
        assert!(matches!(
            &replacement.effect,
            ExecutionAction::SubmitOrder {
                request,
                target_exposure,
            } if request.side == Side::Buy
                && (request.price - 95.0).abs() < f64::EPSILON
                && (request.quantity - test_config().base_qty_per_unit() * 2.0).abs() < f64::EPSILON
                && *target_exposure == Exposure(4.0)
        ));
    }

    #[tokio::test]
    async fn effect_worker_executes_current_submit_when_quantity_rounding_breaks_reverse_exposure_math()
     {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let config = rounded_submit_test_config();
        let mut snapshot = test_snapshot_with_config(config.clone());
        snapshot.current_exposure = Exposure(2.0);
        snapshot.target_exposure = Some(Exposure(3.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        snapshot.observed.reference_price = Some(95.0);
        let state = test_state_with_config(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot.clone()),
            test_budget(),
            config,
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:rounded:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "rounded".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: 3.3,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(3.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        worker.run_once().await.unwrap();

        let submitted_orders = exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted_orders.len(), 1);
        assert!((submitted_orders[0].quantity - 3.3).abs() < 1e-9);

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Succeeded);
    }

    #[tokio::test]
    async fn effect_worker_waits_for_exchange_state_when_receipt_snapshot_has_no_live_order_and_target_not_reached()
     {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(2.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("order-restored"),
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot.clone()),
            test_budget(),
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:recovery:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "recovery".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        worker.run_once().await.unwrap();

        assert!(
            exchange.submitted_orders.lock().unwrap().is_empty(),
            "receipt-backed recovery should wait for live exchange state instead of resubmitting"
        );
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Pending);
        let instance = current_instance(&state).await;
        assert_eq!(
            inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
            Some("order-restored")
        );
    }

    #[tokio::test]
    async fn superseded_recovery_submit_executes_replacement_without_waiting_for_next_poll() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(95.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                test_config().base_qty_per_unit() * 6.0,
                Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot.clone()),
            test_budget(),
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();

        let transition = state
            .write_service
            .observe_position(
                "BTCUSDT",
                super::position_observation(&btc_position(0.0, 0.0)),
            )
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
        assert_eq!(
            current_instance(&state).await.target_exposure,
            Some(Exposure(4.0))
        );

        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:recovery:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "recovery".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: test_config().base_qty_per_unit() * 6.0,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        worker.run_once().await.unwrap();

        let submitted = exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(
            submitted.len(),
            1,
            "replacement submit should run in the same worker iteration"
        );
        assert!(matches!(
            submitted.as_slice(),
            [OrderRequest {
                side: Side::Buy,
                price,
                quantity,
                ..
            }] if (*price - 95.0).abs() < f64::EPSILON
                && (*quantity - test_config().base_qty_per_unit() * 4.0).abs() < f64::EPSILON
        ));
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects
                .iter()
                .find(|effect| effect.effect_id == "BTCUSDT:recovery:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Superseded)
        );
        let replacement = effects
            .iter()
            .find(|effect| effect.effect_id != "BTCUSDT:recovery:0")
            .expect("replacement submit effect should be persisted immediately");
        assert_eq!(replacement.status, EffectStatus::Succeeded);
        assert!(matches!(
            &replacement.effect,
            ExecutionAction::SubmitOrder {
                request,
                target_exposure,
            } if request.side == Side::Buy
                && (request.price - 95.0).abs() < f64::EPSILON
                && (request.quantity - test_config().base_qty_per_unit() * 4.0).abs() < f64::EPSILON
                && *target_exposure == Exposure(4.0)
        ));
    }

    #[tokio::test]
    async fn effect_worker_keeps_receipt_backed_submit_pending_when_attention_required_is_active() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(6.0);
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("order-restored"),
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(22.5, 0.0),
            vec![],
            test_budget(),
        )
        .await;
        fixture
            .persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:recovery:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "recovery".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let handles = fixture.runtime.start().await.unwrap();

        assert!(
            fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
            "attention_required should block duplicate submit attempts"
        );
        let effects = fixture.persistence.all_effects().await;
        assert_eq!(
            effects
                .iter()
                .find(|effect| effect.effect_id == "BTCUSDT:recovery:0")
                .map(|effect| effect.status),
            Some(EffectStatus::Pending)
        );
        let instance = current_instance(&fixture.state).await;
        assert!(inventory_core_order(&instance).is_none());
        assert_eq!(instance.current_exposure, Exposure(6.0));
        assert_eq!(
            instance.executor_state.recovery_anomaly.as_ref(),
            Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn effect_worker_supersedes_submit_when_target_is_reached_without_receipt_evidence() {
        let exchange = Arc::new(FakeExchange::new(btc_position(22.5, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        snapshot.current_exposure = Exposure(6.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(92.5);
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot.clone()),
            test_budget(),
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:recovery:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "recovery".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 92.5,
                        quantity: test_config().base_qty_per_unit() * 6.0,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        worker.run_once().await.unwrap();

        assert!(
            exchange.submitted_orders.lock().unwrap().is_empty(),
            "recovered submit without receipt evidence should not resubmit"
        );
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Superseded);
    }

    #[tokio::test]
    async fn effect_worker_does_not_submit_follow_up_effect_after_failed_cancel_in_same_batch() {
        let exchange = Arc::new(FakeExchange::with_cancel_order_error(
            btc_position(0.0, 0.0),
            vec![],
            "cancel order rejected",
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("snapshot-1"),
                "snapshot-1",
                Side::Buy,
                94.0,
                0.25,
                Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot),
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 90.0)
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { .. },
                ExecutionAction::SubmitOrder { .. }
            ]
        ));

        worker.run_once().await.unwrap();

        assert_eq!(
            exchange.canceled_order_ids.lock().unwrap().as_slice(),
            ["snapshot-1"]
        );
        assert!(
            exchange.submitted_orders.lock().unwrap().is_empty(),
            "submit should stay blocked behind failed cancel"
        );

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].status, EffectStatus::Failed);
        assert_eq!(effects[1].status, EffectStatus::Pending);
    }

    #[tokio::test]
    async fn filled_order_after_failed_cancel_does_not_leave_stale_follow_up_submit_blocking_new_lifecycle()
     {
        let exchange = Arc::new(FakeExchange::with_cancel_order_error(
            btc_position(-22.5, 0.0),
            vec![],
            "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(-6.0);
        snapshot.target_exposure = Some(Exposure(-10.0));
        snapshot.observed.reference_price = Some(105.0);
        set_executor_state(
            &mut snapshot,
            WorkingOrder {
                order_id: Some("order-large-sell".into()),
                client_order_id: "order-large-sell".into(),
                side: Side::Sell,
                price: 106.0,
                quantity: 15.0,
                target_exposure: Exposure(-10.0),
                status: OrderStatus::New,
                role: OrderRole::IncreaseInventory,
            },
            SlotState::Working,
        );
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(snapshot),
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_position(
                "BTCUSDT",
                super::position_observation(&btc_position(-22.5, 0.0)),
            )
            .await
            .unwrap();
        assert!(matches!(
            transition.effects.as_slice(),
            [
                ExecutionAction::CancelOrder { order_id, .. },
                ExecutionAction::SubmitOrder { request, .. }
            ] if order_id == "order-large-sell"
                && request.reduce_only
                && request.side == Side::Buy
        ));

        worker.run_once().await.unwrap();

        let effects = persistence.all_effects().await;
        assert!(
            effects
                .iter()
                .all(|effect| {
                    !(effect.status == EffectStatus::Pending
                        && matches!(effect.effect, ExecutionAction::SubmitOrder { .. }))
                }),
            "old lifecycle should not leave a pending submit behind after new lifecycle executes"
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| effect.status == EffectStatus::Superseded)
                .count(),
            1,
            "stale follow-up submit should be retired instead of staying pending"
        );
        assert_eq!(exchange.submitted_orders.lock().unwrap().len(), 1);

        state
            .write_service
            .observe_order_with_absorb_result(
                "BTCUSDT",
                super::order_observation(&btc_exchange_order(
                    "order-large-sell",
                    "order-large-sell",
                    Side::Sell,
                    106.0,
                    15.0,
                    0.0,
                    OrderStatus::Filled,
                )),
            )
            .await
            .unwrap();

        let effects_after_terminal_update = persistence.all_effects().await;
        assert!(
            effects_after_terminal_update
                .iter()
                .all(|effect| {
                    !(effect.status == EffectStatus::Pending
                        && matches!(effect.effect, ExecutionAction::SubmitOrder { .. }))
                }),
            "terminal update should not resurrect stale follow-up submits"
        );
    }

    #[tokio::test]
    async fn effect_worker_keeps_effect_pending_when_submit_cleanup_persistence_fails() {
        let exchange = Arc::new(FakeExchange::with_submit_error(
            btc_position(0.0, 0.0),
            vec![],
            "submit rejected",
        ));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        let transition = state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
        );

        worker.run_once().await.unwrap();

        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Pending);
        assert_eq!(effects[0].attempt_count, 0);

        let instance = current_instance(&state).await;
        assert_eq!(
            inventory_core_order(&instance).map(|order| order.status),
            Some(OrderStatus::Submitting)
        );
    }

    #[tokio::test]
    async fn recovered_submit_emits_effect_state_changed_notification() {
        let exchange = Arc::new(FakeExchange::new(
            btc_position(0.0, 0.0),
            vec![btc_exchange_order(
                "order-restored",
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                0.0,
                OrderStatus::New,
            )],
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let mut restored_snapshot = test_snapshot();
        set_executor_state(
            &mut restored_snapshot,
            working_order(
                Some("order-restored"),
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            Some(restored_snapshot),
            test_budget(),
        )
        .await;
        persistence
            .save_transition("BTCUSDT", &current_instance(&state).await, &[], &[])
            .await
            .unwrap();
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:recovery:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "recovery".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );
        let mut receiver = state.write_service.subscribe_notifications();

        worker.run_once().await.unwrap();

        let mut saw_effect_state_changed = false;
        for _ in 0..3 {
            let event = timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(
                event,
                crate::notifications::TrackInternalNotification::TrackEffectStateChanged { .. }
            ) {
                saw_effect_state_changed = true;
                break;
            }
        }

        assert!(saw_effect_state_changed);
    }

    #[tokio::test]
    async fn receipt_persistence_failure_emits_effect_state_changed_notification() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(FailOnReceiptPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );
        let mut receiver = state.write_service.subscribe_notifications();

        state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        let committed = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            committed,
            crate::notifications::TrackInternalNotification::TrackWriteCommitted { .. }
        ));
        worker.run_once().await.unwrap();

        let committed = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            committed,
            crate::notifications::TrackInternalNotification::TrackEffectStateChanged { .. }
        ));
    }

    #[tokio::test]
    async fn effect_worker_keeps_effect_pending_while_submit_is_inflight() {
        let submit_started = Arc::new(Notify::new());
        let release_submit = Arc::new(Notify::new());
        let exchange = Arc::new(FakeExchange::with_blocked_submit(
            btc_position(0.0, 0.0),
            vec![],
            submit_started.clone(),
            release_submit.clone(),
        ));
        let persistence = Arc::new(MemoryPersistence::default());
        let state = test_state(
            exchange.clone() as Arc<dyn ExchangePort>,
            persistence.clone(),
            None,
            test_budget(),
        )
        .await;
        let worker = EffectWorker::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );

        state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();

        let task = tokio::spawn({
            let worker = worker.clone();
            async move { worker.run_once().await }
        });

        submit_started.notified().await;
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].status, EffectStatus::Pending);

        release_submit.notify_waiters();
        task.await.unwrap().unwrap();
    }

    #[derive(Clone, Default)]
    struct SharedLogBuffer(StdArc<Mutex<Vec<u8>>>);

    struct SharedLogWriter(StdArc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for SharedLogBuffer {
        type Writer = SharedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedLogWriter(StdArc::clone(&self.0))
        }
    }

    impl io::Write for SharedLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn effect_worker_reports_missing_loaded_track_for_effect_writeback() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:batch:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "batch".into(),
                sequence: 0,
                effect: ExecutionAction::CancelOrder {
                    instrument: btc_instrument(),
                    order_id: "order-1".into(),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let manager = TrackManager::new(clock);
        let (events, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = persistence.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = persistence;
        let state = build_server_state(
            Arc::new(TrackWriteService::new(
                manager,
                state_repository.clone(),
                events,
            )),
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
            Arc::new(TrackProjector::new()),
        );
        let worker = EffectWorker::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            Duration::from_millis(10),
        );
        let logs = SharedLogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        worker.run_once().await.unwrap();

        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("loaded-track invariant violated"));
        assert!(!captured.contains("submit order failed"));
    }

    #[tokio::test]
    async fn position_update_reconciles_actual_exposure_without_overwriting_target() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until_instance(&fixture.state, |instance| {
            instance
                .target_exposure
                .as_ref()
                .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
                .unwrap_or(false)
        })
        .await;

        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                11.0,
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
                && instance
                    .target_exposure
                    .as_ref()
                    .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
                    .unwrap_or(false)
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert!((instance.risk.unrealized_pnl - 11.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn position_update_reconciles_without_runtime_follow_up_command() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = TrackManager::new(clock);
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        snapshot.observed.reference_price = Some(95.0);
        manager.restore_track_state(&snapshot).unwrap();
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            persistence.clone(),
            Arc::new(TrackQueryService::new(
                persistence.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let user_task = runtime.spawn_user_task(
            user_receiver,
            test_server_time(),
            runtime.shutdown_tx.subscribe(),
        );
        let save_count_before_event = persistence.save_transition_count();
        user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                11.0,
            ))
            .await
            .unwrap();

        wait_until_async(|| {
            let persistence = persistence.clone();
            async move { persistence.save_transition_count() == save_count_before_event + 1 }
        })
        .await;

        assert_eq!(
            persistence.save_transition_count() - save_count_before_event,
            1
        );
        let effects = persistence.all_effects().await;
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].effect,
            ExecutionAction::SubmitOrder { .. }
        ));
        assert!(exchange.submitted_orders.lock().unwrap().is_empty());

        user_task.abort();
        let _ = user_task.await;
    }

    #[tokio::test]
    async fn position_update_submits_reconcile_without_waiting_for_new_tick() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(4.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        snapshot.observed.reference_price = Some(95.0);

        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
        let clock = Arc::new(FixedClock(test_server_time()));

        let mut manager = TrackManager::new(clock);
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();
        manager.restore_track_state(&snapshot).unwrap();
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            persistence.clone(),
            Arc::new(TrackQueryService::new(
                persistence.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );
        let runtime = ServerRuntime::new(
            state.clone(),
            exchange.clone() as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let user_task = runtime.spawn_user_task(
            user_receiver,
            test_server_time(),
            runtime.shutdown_tx.subscribe(),
        );
        let effect_task = runtime.spawn_effect_task(runtime.shutdown_tx.subscribe());
        user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                11.0,
            ))
            .await
            .unwrap();

        wait_until(|| exchange.submitted_orders.lock().unwrap().len() == 1).await;
        wait_until_instance(&state, |instance| {
            inventory_core_order(instance).and_then(|order| order.order_id.as_deref())
                == Some("order-1")
        })
        .await;

        let submitted = exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted[0].side, Side::Buy);
        assert_eq!(submitted[0].quantity, 7.5);

        user_task.abort();
        let _ = user_task.await;
        effect_task.abort();
        let _ = effect_task.await;
    }

    #[tokio::test]
    async fn position_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(0.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        snapshot.observed.reference_price = Some(100.0);
        snapshot.risk.unrealized_pnl = 0.0;

        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        let mut receiver = fixture.state.write_service.subscribe_notifications();
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                0.0,
                11.0,
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::notifications::TrackInternalNotification::TrackWriteCommitted { .. }
        ));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn order_update_clears_inventory_core_slot_on_terminal_status() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until_instance(&fixture.state, |instance| {
            inventory_core_order(instance)
                .and_then(|order| order.order_id.as_deref())
                .is_some()
        })
        .await;

        let order = inventory_core_order(&current_instance(&fixture.state).await)
            .unwrap()
            .clone();

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    &order.order_id.clone().unwrap(),
                    &order.client_order_id,
                    Side::Buy,
                    order.price,
                    order.quantity,
                    0.0,
                    OrderStatus::Filled,
                ),
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            inventory_core_order(instance).is_none()
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn terminal_order_update_reconciles_without_waiting_for_new_tick() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

        let order = inventory_core_order(&current_instance(&fixture.state).await)
            .unwrap()
            .clone();

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    &order.order_id.clone().unwrap(),
                    &order.client_order_id,
                    Side::Buy,
                    order.price,
                    order.quantity,
                    0.0,
                    OrderStatus::Canceled,
                ),
            ))
            .await
            .unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 2).await;
        wait_until_instance(&fixture.state, |instance| {
            inventory_core_order(instance)
                .and_then(|working_order| working_order.order_id.as_deref())
                == Some("order-2")
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn terminal_order_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event()
     {
        let mut snapshot = TrackSnapshot {
            track_id: TrackId::new("BTCUSDT"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config: test_config(),
            status: TrackStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(0.0)),
            manual_target_override: None,
            executor_state: ExecutorState::empty(test_server_time()),
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: poise_engine::snapshot::ObservedState {
                reference_price: Some(100.0),
                out_of_band_since: None,
                last_tick_at: None,
                market_data_stale_since: None,
            },
        };
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("order-1"),
                "order-1",
                Side::Buy,
                100.0,
                0.1,
                Exposure(0.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let open_orders = vec![ExchangeOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
            client_order_id: "order-1".into(),
            side: Side::Buy,
            price: 100.0,
            qty: 0.1,
            realized_pnl: 0.0,
            status: OrderStatus::New,
        }];
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            open_orders,
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        let mut receiver = fixture.state.write_service.subscribe_notifications();
        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    "order-1",
                    "order-1",
                    Side::Buy,
                    100.0,
                    0.1,
                    0.0,
                    OrderStatus::Canceled,
                ),
            ))
            .await
            .unwrap();

        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            crate::notifications::TrackInternalNotification::TrackWriteCommitted { .. }
        ));

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_restores_claimed_live_order_before_replanning() {
        let snapshot = test_snapshot();
        let live_order = btc_exchange_order(
            "snapshot-1",
            "snapshot-1",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![live_order],
            test_budget(),
        )
        .await;
        let save_count_before_start = fixture.persistence.save_transition_count();

        fixture.runtime.startup_sync().await.unwrap();
        assert_eq!(
            fixture.persistence.save_transition_count() - save_count_before_start,
            1,
            "startup sync should persist live exchange state through a single write path"
        );

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert_eq!(
            instance.observed.out_of_band_since,
            Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap())
        );
        let executor_state = &instance.executor_state;
        assert_eq!(
            executor_state.slots.as_slice(),
            [poise_engine::runtime::ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(poise_engine::runtime::WorkingOrder {
                    order_id: Some("snapshot-1".into()),
                    client_order_id: "snapshot-1".into(),
                    side: Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    target_exposure: Exposure(6.0),
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }]
        );
        let effects = fixture.persistence.all_effects().await;
        assert!(effects.iter().any(|effect| {
            matches!(
                &effect.effect,
                ExecutionAction::CancelOrder { order_id, .. } if order_id == "snapshot-1"
            )
        }));
        assert!(effects.iter().any(|effect| {
            matches!(
                &effect.effect,
                ExecutionAction::SubmitOrder { request, target_exposure }
                    if request.client_order_id.starts_with("BTCUSDT-")
                        && (request.price - 95.0).abs() < f64::EPSILON
                        && (request.quantity - 7.5).abs() < f64::EPSILON
                        && *target_exposure == Exposure(4.0)
            )
        }));
    }

    #[tokio::test]
    async fn startup_sync_replans_even_when_pending_submit_effect_is_present() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "snapshot-1",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
        )
        .await;
        fixture
            .persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:startup:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "startup".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "snapshot-1".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        fixture.runtime.startup_sync().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(
            inventory_core_order(&instance)
                .map(|order| order.client_order_id.starts_with("BTCUSDT-")),
            Some(true)
        );

        let effects = fixture.persistence.all_effects().await;
        assert!(effects.iter().any(|effect| {
            matches!(
                &effect.effect,
                ExecutionAction::SubmitOrder { request, target_exposure }
                    if request.client_order_id.starts_with("BTCUSDT-")
                        && (request.price - 95.0).abs() < f64::EPSILON
                        && (request.quantity - 15.0).abs() < f64::EPSILON
                        && *target_exposure == Exposure(4.0)
            )
        }));
    }

    #[tokio::test]
    async fn startup_sync_does_not_duplicate_matching_pending_submit_effect() {
        let mut snapshot = test_snapshot();
        snapshot.current_exposure = Exposure(0.0);
        snapshot.target_exposure = Some(Exposure(6.0));
        snapshot.observed.reference_price = Some(92.5);
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "BTCUSDT-reconcile",
                Side::Buy,
                92.5,
                22.5,
                Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
        )
        .await;
        fixture
            .persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:startup:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "startup".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 92.5,
                        quantity: 22.5,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        fixture.runtime.startup_sync().await.unwrap();

        let pending_effects = fixture
            .persistence
            .list_dispatchable_effects()
            .await
            .unwrap();
        assert_eq!(pending_effects.len(), 1);
        assert!(matches!(
            pending_effects.as_slice(),
            [PersistedTrackEffect {
                effect:
                    ExecutionAction::SubmitOrder {
                        request,
                        target_exposure,
                    },
                ..
            }] if request.client_order_id == "BTCUSDT-reconcile"
                && (request.price - 92.5).abs() < f64::EPSILON
                && (request.quantity - 22.5).abs() < f64::EPSILON
                && *target_exposure == Exposure(6.0)
        ));
    }

    #[tokio::test]
    async fn startup_sync_marks_attention_required_when_live_order_cannot_be_claimed() {
        let mut snapshot = test_snapshot();
        snapshot.target_exposure = Some(Exposure(0.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![btc_exchange_order(
                "live-1",
                "unexpected-live",
                Side::Buy,
                94.5,
                0.25,
                0.0,
                OrderStatus::New,
            )],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(0.0));
        assert_eq!(instance.target_exposure, Some(Exposure(0.0)));
        assert_eq!(
            instance.executor_state.recovery_anomaly.as_ref(),
            Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_rebuilds_inventory_core_slot_when_exchange_has_no_open_orders() {
        let fixture = runtime_fixture(
            Some(test_snapshot()),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;

        fixture.runtime.startup_sync().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert_eq!(instance.target_exposure, Some(Exposure(4.0)));
        assert_eq!(
            inventory_core_order(&instance)
                .map(|order| order.client_order_id.starts_with("BTCUSDT-")),
            Some(true)
        );
        assert_ne!(
            inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
            Some("snapshot-1")
        );
    }

    #[tokio::test]
    async fn startup_sync_rebuilds_submit_pending_slot_to_current_plan_before_follow_up_sync() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;
        fixture
            .persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:startup:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "startup".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;
        fixture.runtime.startup_sync().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        let order = inventory_core_order(&instance).expect("expected submit pending working order");
        assert!(order.client_order_id.starts_with("BTCUSDT-"));
        assert_eq!(order.order_id, None);
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.price, 95.0);
        assert_eq!(order.quantity, 7.5);
        assert_eq!(order.target_exposure, Exposure(4.0));
        assert_eq!(order.status, OrderStatus::Submitting);

        let transition = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
    }

    #[tokio::test]
    async fn startup_sync_marks_attention_required_when_receipt_backed_submit_has_no_live_order() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("receipt-1"),
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;
        fixture
            .persistence
            .seed_effect(PersistedTrackEffect {
                effect_id: "BTCUSDT:startup:0".into(),
                track_id: TrackId::new("BTCUSDT"),
                batch_id: "startup".into(),
                sequence: 0,
                effect: ExecutionAction::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 94.0,
                        quantity: 0.25,
                        client_order_id: "BTCUSDT-reconcile".into(),
                        reduce_only: false,
                    },
                    target_exposure: Exposure(6.0),
                },
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            })
            .await;

        let handles = fixture.runtime.start().await.unwrap();

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref()
                == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        })
        .await;
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_clears_orphaned_submit_pending_slot_without_effect() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                None,
                "BTCUSDT-reconcile",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![],
            test_budget(),
        )
        .await;

        fixture.runtime.startup_sync().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(
            inventory_core_order(&instance)
                .map(|order| order.client_order_id.starts_with("BTCUSDT-")),
            Some(true)
        );

        let transition = fixture
            .state
            .write_service
            .observe_market("BTCUSDT", 95.0)
            .await
            .unwrap();
        assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
    }

    #[tokio::test]
    async fn startup_sync_rebuilds_multiple_live_open_orders_when_they_match_distinct_slots() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("order-a"),
                "client-a",
                Side::Buy,
                94.5,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        snapshot.executor_state.slots.push(ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::Working,
            working_order: Some(working_order(
                Some("order-b"),
                "client-b",
                Side::Sell,
                95.5,
                0.15,
                Exposure(2.0),
                OrderStatus::PartiallyFilled,
            )),
        });
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![
                btc_exchange_order(
                    "order-b",
                    "client-b",
                    Side::Sell,
                    95.5,
                    0.15,
                    0.0,
                    OrderStatus::New,
                ),
                btc_exchange_order(
                    "order-a",
                    "client-a",
                    Side::Buy,
                    94.5,
                    0.25,
                    0.0,
                    OrderStatus::PartiallyFilled,
                ),
            ],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        assert!(
            fixture
                .exchange
                .cancel_all_symbols
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());
        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert!(instance.executor_state.recovery_anomaly.is_none());
        assert_eq!(instance.executor_state.slots.len(), 2);
        assert_eq!(
            instance.executor_state.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-a")
        );
        assert_eq!(
            instance.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-b")
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn shutdown_cancels_orders_and_persists_final_exchange_state() {
        let mut snapshot = test_snapshot();
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("live-1"),
                "live-1",
                Side::Buy,
                94.5,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let fixture = runtime_fixture(
            Some(snapshot),
            btc_position(7.5, 3.0),
            vec![btc_exchange_order(
                "live-1",
                "live-1",
                Side::Buy,
                94.5,
                0.25,
                0.0,
                OrderStatus::New,
            )],
            test_budget(),
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();

        fixture.runtime.shutdown(handles).await;

        assert_eq!(
            fixture
                .exchange
                .cancel_all_symbols
                .lock()
                .unwrap()
                .as_slice(),
            ["BTCUSDT"]
        );
        let snapshot = fixture
            .persistence
            .load_track_state("BTCUSDT")
            .await
            .unwrap()
            .expect("final snapshot should be persisted");
        assert_eq!(snapshot.current_exposure, Exposure(2.0));
        assert_eq!(snapshot.executor_state.recovery_anomaly, None);
        assert_eq!(
            snapshot.executor_state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }]
        );
    }

    #[tokio::test]
    async fn recovery_task_resyncs_recovery_anomaly_automatically_without_user_data() {
        let mut snapshot = test_snapshot();
        snapshot.target_exposure = Some(Exposure(0.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        let fixture = runtime_fixture_with_recovery_retry_interval(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![btc_exchange_order(
                "live-1",
                "unexpected-live",
                Side::Buy,
                94.5,
                0.25,
                0.0,
                OrderStatus::New,
            )],
            test_budget(),
            Duration::from_millis(50),
        )
        .await;

        let RuntimeHandles {
            market_task,
            user_task,
            effect_task,
            recovery_task,
        } = fixture.runtime.start().await.unwrap();
        market_task.abort();
        let _ = market_task.await;
        effect_task.abort();
        let _ = effect_task.await;

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref()
                == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        })
        .await;
        assert_eq!(
            fixture.exchange.get_position_calls.load(Ordering::SeqCst),
            1
        );
        assert_eq!(
            fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst),
            1
        );

        wait_until(|| {
            fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst)
                >= 2
        })
        .await;
        assert!(fixture.exchange.get_position_calls.load(Ordering::SeqCst) >= 2);
        assert!(
            fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst)
                >= 2
        );

        fixture.exchange.open_orders.lock().unwrap().clear();

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref().is_none()
        })
        .await;
        assert!(fixture.exchange.get_position_calls.load(Ordering::SeqCst) >= 3);
        assert!(
            fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst)
                >= 3
        );
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        recovery_task.abort();
        let _ = recovery_task.await;
        user_task.abort();
        let _ = user_task.await;
    }

    #[tokio::test]
    async fn recovery_task_cancels_unknown_live_orders_automatically() {
        let mut snapshot = test_snapshot();
        snapshot.target_exposure = Some(Exposure(0.0));
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        let fixture = runtime_fixture_with_recovery_retry_interval(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![
                btc_exchange_order(
                    "live-1",
                    "unexpected-live-1",
                    Side::Buy,
                    94.5,
                    0.25,
                    0.0,
                    OrderStatus::New,
                ),
                btc_exchange_order(
                    "live-2",
                    "unexpected-live-2",
                    Side::Buy,
                    94.6,
                    0.25,
                    0.0,
                    OrderStatus::New,
                ),
            ],
            test_budget(),
            Duration::from_millis(50),
        )
        .await;

        let RuntimeHandles {
            market_task,
            user_task,
            effect_task,
            recovery_task,
        } = fixture.runtime.start().await.unwrap();
        market_task.abort();
        let _ = market_task.await;
        effect_task.abort();
        let _ = effect_task.await;

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref()
                == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        })
        .await;

        wait_until(|| fixture.exchange.canceled_order_ids.lock().unwrap().len() >= 2).await;
        assert_eq!(
            fixture.exchange.canceled_order_ids.lock().unwrap().as_slice(),
            ["live-1", "live-2"]
        );

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref().is_none()
        })
        .await;
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        recovery_task.abort();
        let _ = recovery_task.await;
        user_task.abort();
        let _ = user_task.await;
    }

    #[tokio::test]
    async fn background_health_check_marks_market_data_stale_without_follow_up_events() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap();
        let clock = Arc::new(MutableClock(Arc::new(Mutex::new(started_at))));
        let mut snapshot = test_snapshot();
        snapshot.status = TrackStatus::Paused;
        snapshot.target_exposure = None;
        snapshot.executor_state = ExecutorState::empty(test_server_time());
        let fixture = runtime_fixture_with_clock_and_recovery_retry_interval(
            Some(snapshot),
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
            Duration::from_millis(50),
            Duration::from_secs(5),
            clock.clone() as Arc<dyn ClockPort>,
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

        wait_until_instance(&fixture.state, |instance| {
            instance.observed.last_tick_at.is_some()
        })
        .await;

        clock.set(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 31).unwrap());

        wait_until_instance(&fixture.state, |instance| {
            instance.observed.market_data_stale_since.is_some()
        })
        .await;

        let instance = current_instance(&fixture.state).await;
        assert!(instance.observed.market_data_stale_since.is_some());
        assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_replays_buffered_user_event_before_first_tick() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                5.0,
            ))
            .await
            .unwrap();

        let handles = fixture.runtime.start().await.unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn startup_sync_ignores_buffered_user_event_older_than_cutoff() {
        let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() - chrono::Duration::milliseconds(1),
                3.75,
                9.0,
            ))
            .await
            .unwrap();

        let handles = fixture.runtime.start().await.unwrap();

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert!((instance.risk.unrealized_pnl - 3.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn runtime_start_fails_when_buffered_user_data_replay_cannot_be_persisted() {
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(FailOnSavePersistence::new(2));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = TrackManager::new(clock);
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            persistence.clone(),
            Arc::new(TrackQueryService::new(
                persistence.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );
        let runtime = ServerRuntime::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );
        user_sender
            .send(position_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                7.5,
                5.0,
            ))
            .await
            .unwrap();

        let error = runtime.start().await.err().unwrap();
        assert!(error.to_string().contains("injected save failure"));
    }

    #[tokio::test]
    async fn apply_user_data_event_preserves_write_service_mutation_error_kind() {
        let manager = TrackManager::new(Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        )));
        let persistence = Arc::new(MemoryPersistence::default());
        let (events, _) = broadcast::channel(16);
        let state = build_server_state(
            Arc::new(TrackWriteService::new(
                manager,
                persistence.clone() as Arc<dyn StateRepositoryPort>,
                events.clone(),
            )),
            persistence.clone() as Arc<dyn StateRepositoryPort>,
            Arc::new(TrackQueryService::new(
                persistence as Arc<dyn TrackReadRepositoryPort>,
            )),
            Arc::new(TrackProjector::new()),
        );

        let error = super::apply_user_data_event(
            &state,
            &(Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![])) as Arc<dyn ExchangePort>),
            "missing-track",
            position_event_at(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 1).unwrap(),
                1.0,
                0.0,
            ),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            crate::write_service::TrackMutationError::Mutation(_)
        ));
    }

    #[tokio::test]
    async fn stale_live_user_event_does_not_rollback_state_after_start() {
        let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .user_sender
            .send(position_event_at(
                test_server_time() - chrono::Duration::milliseconds(1),
                3.75,
                9.0,
            ))
            .await
            .unwrap();
        sleep(Duration::from_millis(100)).await;

        let instance = current_instance(&fixture.state).await;
        assert_eq!(instance.current_exposure, Exposure(2.0));
        assert!((instance.risk.unrealized_pnl - 3.0).abs() < f64::EPSILON);

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn filled_order_updates_realized_pnl_and_trips_daily_loss_cap() {
        let fixture = runtime_fixture(
            None,
            btc_position(7.5, 0.0),
            vec![],
            CapacityBudget {
                max_notional: 3000.0,
                daily_loss_limit: -10.0,
                stop_loss_pct: 10.0,
            },
        )
        .await;

        let handles = fixture.runtime.start().await.unwrap();
        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    "fill-1",
                    "fill-1",
                    Side::Sell,
                    95.0,
                    7.5,
                    -20.0,
                    OrderStatus::Filled,
                ),
            ))
            .await
            .unwrap();

        wait_until_instance(&fixture.state, |instance| {
            (instance.risk.realized_pnl_today + 20.0).abs() < f64::EPSILON
        })
        .await;

        fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

        wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

        let submitted = fixture.exchange.submitted_orders.lock().unwrap().clone();
        assert_eq!(submitted[0].side, Side::Sell);
        assert_eq!(
            current_instance(&fixture.state).await.target_exposure,
            Some(Exposure(0.0))
        );

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn unabsorbed_order_update_triggers_immediate_reconcile() {
        let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
        let handles = fixture.runtime.start().await.unwrap();
        let position_calls_before = fixture.exchange.get_position_calls.load(Ordering::SeqCst);
        let open_orders_calls_before = fixture.exchange.get_open_orders_calls.load(Ordering::SeqCst);

        fixture
            .user_sender
            .send(order_event_at(
                test_server_time() + chrono::Duration::milliseconds(1),
                btc_exchange_order(
                    "untracked-live-order",
                    "untracked-live-order",
                    Side::Buy,
                    95.0,
                    1.0,
                    0.0,
                    OrderStatus::New,
                ),
            ))
            .await
            .unwrap();

        wait_until(|| {
            fixture.exchange.get_position_calls.load(Ordering::SeqCst) > position_calls_before
                && fixture.exchange.get_open_orders_calls.load(Ordering::SeqCst)
                    > open_orders_calls_before
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn normal_track_low_frequency_reconcile_discovers_untracked_live_orders_without_restart() {
        let fixture = runtime_fixture_with_intervals(
            None,
            btc_position(0.0, 0.0),
            vec![],
            test_budget(),
            Duration::from_secs(1),
            Duration::from_millis(50),
        )
        .await;
        let handles = fixture.runtime.start().await.unwrap();

        fixture.exchange.set_open_orders(vec![btc_exchange_order(
            "live-1",
            "unexpected-live-1",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        )]);

        wait_until_instance(&fixture.state, |instance| {
            instance.executor_state.recovery_anomaly.as_ref()
                == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
        })
        .await;

        shutdown(handles).await;
    }

    #[tokio::test]
    async fn runtime_start_fails_when_user_data_subscription_cannot_be_created() {
        let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        drop(price_sender);
        let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));

        let mut manager = TrackManager::new(clock);
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                test_budget(),
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            persistence.clone(),
            Arc::new(TrackQueryService::new(
                persistence.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );

        let runtime = ServerRuntime::new(
            state,
            exchange as Arc<dyn ExchangePort>,
            market_data as Arc<dyn MarketDataPort>,
        );

        let error = runtime.start().await.err().unwrap();
        assert!(error.to_string().contains("missing test user receiver"));
    }

    struct RuntimeFixture {
        runtime: ServerRuntime,
        state: ServerState,
        exchange: Arc<FakeExchange>,
        persistence: Arc<MemoryPersistence>,
        price_sender: mpsc::Sender<PriceTick>,
        user_sender: mpsc::Sender<UserDataEvent>,
    }

    async fn runtime_fixture(
        restored_snapshot: Option<TrackSnapshot>,
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        budget: CapacityBudget,
    ) -> RuntimeFixture {
        runtime_fixture_with_intervals(
            restored_snapshot,
            position,
            open_orders,
            budget,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .await
    }

    async fn runtime_fixture_with_recovery_retry_interval(
        restored_snapshot: Option<TrackSnapshot>,
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        budget: CapacityBudget,
        recovery_retry_interval: Duration,
    ) -> RuntimeFixture {
        runtime_fixture_with_intervals(
            restored_snapshot,
            position,
            open_orders,
            budget,
            recovery_retry_interval,
            Duration::from_secs(5),
        )
        .await
    }

    async fn runtime_fixture_with_intervals(
        restored_snapshot: Option<TrackSnapshot>,
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        budget: CapacityBudget,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
    ) -> RuntimeFixture {
        runtime_fixture_with_clock_and_recovery_retry_interval(
            restored_snapshot,
            position,
            open_orders,
            budget,
            recovery_retry_interval,
            audit_interval,
            Arc::new(FixedClock(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            )),
        )
        .await
    }

    async fn runtime_fixture_with_clock_and_recovery_retry_interval(
        restored_snapshot: Option<TrackSnapshot>,
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        budget: CapacityBudget,
        recovery_retry_interval: Duration,
        audit_interval: Duration,
        clock: Arc<dyn ClockPort>,
    ) -> RuntimeFixture {
        let exchange = Arc::new(FakeExchange::new(position, open_orders));
        let persistence = Arc::new(MemoryPersistence::default());
        let (price_sender, price_receiver) = mpsc::channel(8);
        let (user_sender, user_receiver) = mpsc::channel(8);
        let market_data = Arc::new(FakeMarketData::new(price_receiver, user_receiver));

        let mut manager = TrackManager::new(clock);
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                test_config(),
                budget,
                exchange.exchange_info.rules.clone(),
            )
            .unwrap();

        if let Some(snapshot) = restored_snapshot.clone() {
            manager.restore_track_state(&snapshot).unwrap();
            persistence
                .save_transition("BTCUSDT", &snapshot, &[], &[])
                .await
                .unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            persistence.clone(),
            events.clone(),
        ));
        let state = build_server_state(
            write_service,
            persistence.clone(),
            Arc::new(TrackQueryService::new(
                persistence.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );

        RuntimeFixture {
            runtime: ServerRuntime::with_reconcile_intervals(
                state.clone(),
                exchange.clone() as Arc<dyn ExchangePort>,
                market_data as Arc<dyn MarketDataPort>,
                recovery_retry_interval,
                audit_interval,
            ),
            state,
            exchange,
            persistence,
            price_sender,
            user_sender,
        }
    }

    async fn test_state<R>(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<R>,
        restored_snapshot: Option<TrackSnapshot>,
        budget: CapacityBudget,
    ) -> ServerState
    where
        R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
    {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = TrackManager::new(clock);
        let instrument = btc_instrument();
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                instrument.clone(),
                test_config(),
                budget,
                exchange.get_exchange_info(&instrument).await.unwrap().rules,
            )
            .unwrap();
        if let Some(snapshot) = restored_snapshot {
            manager.restore_track_state(&snapshot).unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = persistence.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = persistence;
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            state_repository.clone(),
            events.clone(),
        ));
        build_server_state(
            write_service,
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
            Arc::new(TrackProjector::new()),
        )
    }

    async fn test_state_with_config<R>(
        exchange: Arc<dyn ExchangePort>,
        persistence: Arc<R>,
        restored_snapshot: Option<TrackSnapshot>,
        budget: CapacityBudget,
        config: TrackConfig,
    ) -> ServerState
    where
        R: StateRepositoryPort + TrackReadRepositoryPort + 'static,
    {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = TrackManager::new(clock);
        let instrument = btc_instrument();
        manager
            .add_track(
                TrackId::new("BTCUSDT"),
                instrument.clone(),
                config,
                budget,
                exchange.get_exchange_info(&instrument).await.unwrap().rules,
            )
            .unwrap();
        if let Some(snapshot) = restored_snapshot {
            manager.restore_track_state(&snapshot).unwrap();
        }

        let (events, _) = broadcast::channel(16);
        let state_repository: Arc<dyn StateRepositoryPort> = persistence.clone();
        let read_repository: Arc<dyn TrackReadRepositoryPort> = persistence;
        let write_service = Arc::new(TrackWriteService::new(
            manager,
            state_repository.clone(),
            events.clone(),
        ));
        build_server_state(
            write_service,
            state_repository,
            Arc::new(TrackQueryService::new(read_repository)),
            Arc::new(TrackProjector::new()),
        )
    }

    async fn current_instance(state: &ServerState) -> poise_engine::snapshot::TrackRuntimeSnapshot {
        let manager_handle = state.write_service.manager();
        let manager = manager_handle.read().await;
        manager.get_track("BTCUSDT").unwrap().snapshot()
    }

    async fn shutdown(handles: RuntimeHandles) {
        handles.market_task.abort();
        handles.user_task.abort();
        handles.effect_task.abort();
        handles.recovery_task.abort();
        let _ = handles.market_task.await;
        let _ = handles.user_task.await;
        let _ = handles.effect_task.await;
        let _ = handles.recovery_task.await;
    }

    async fn wait_until<F>(condition: F)
    where
        F: Fn() -> bool,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if condition() {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_instance<F>(state: &ServerState, predicate: F)
    where
        F: Fn(&poise_engine::snapshot::TrackRuntimeSnapshot) -> bool,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if predicate(&current_instance(state).await) {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_until_async<F, Fut>(condition: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = bool>,
    {
        timeout(Duration::from_secs(1), async {
            loop {
                if condition().await {
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    fn ready_pending_effects(effects: &[PersistedTrackEffect]) -> Vec<PersistedTrackEffect> {
        effects
            .iter()
            .filter(|effect| {
                effect.status == EffectStatus::Pending
                    && !effects.iter().any(|prior| {
                        prior.track_id == effect.track_id
                            && prior.batch_id == effect.batch_id
                            && prior.sequence < effect.sequence
                            && !prior.status.unblocks_follow_up()
                    })
            })
            .cloned()
            .collect()
    }

    fn apply_effect_status_update(
        effects: &mut [PersistedTrackEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
        now: chrono::DateTime<Utc>,
    ) -> Result<()> {
        let Some(effect_status_update) = effect_status_update else {
            return Ok(());
        };
        let effect = effects
            .iter_mut()
            .find(|effect| effect.effect_id == effect_status_update.effect_id)
            .ok_or_else(|| anyhow!("effect `{}` not found", effect_status_update.effect_id))?;
        effect.status = effect_status_update.status;
        effect.attempt_count += effect_status_update.attempt_delta;
        effect.last_error = effect_status_update.last_error.clone();
        effect.updated_at = now;
        Ok(())
    }

    fn test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn rounded_submit_test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 6.0,
            short_exposure_units: 6.0,
            notional_per_unit: 333.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 10.0,
        }
    }

    fn test_server_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()
    }

    fn btc_instrument() -> Instrument {
        Instrument::new(Venue::Binance, "BTCUSDT")
    }

    fn btc_position(qty: f64, unrealized_pnl: f64) -> Position {
        Position {
            instrument: btc_instrument(),
            qty,
            avg_price: 100.0,
            unrealized_pnl,
        }
    }

    fn btc_tick(reference_price: f64) -> PriceTick {
        PriceTick {
            instrument: btc_instrument(),
            reference_price,
            mark_price: reference_price,
            timestamp: Utc::now(),
        }
    }

    fn btc_exchange_order(
        order_id: &str,
        client_order_id: &str,
        side: Side,
        price: f64,
        qty: f64,
        realized_pnl: f64,
        status: OrderStatus,
    ) -> ExchangeOrder {
        ExchangeOrder {
            instrument: btc_instrument(),
            order_id: order_id.into(),
            client_order_id: client_order_id.into(),
            side,
            price,
            qty,
            realized_pnl,
            status,
        }
    }

    fn position_event_at(
        event_time: chrono::DateTime<Utc>,
        qty: f64,
        unrealized_pnl: f64,
    ) -> UserDataEvent {
        UserDataEvent {
            event_time,
            payload: UserDataPayload::PositionUpdate(btc_position(qty, unrealized_pnl)),
        }
    }

    fn order_event_at(event_time: chrono::DateTime<Utc>, order: ExchangeOrder) -> UserDataEvent {
        UserDataEvent {
            event_time,
            payload: UserDataPayload::OrderUpdate(order),
        }
    }

    fn test_snapshot() -> TrackSnapshot {
        test_snapshot_with_config(test_config())
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

    fn set_executor_state(snapshot: &mut TrackSnapshot, order: WorkingOrder, state: SlotState) {
        snapshot.executor_state = ExecutorState {
            mode: ExecutionMode::Passive,
            inventory_gap: snapshot.current_exposure.delta(&order.target_exposure),
            gap_started_at: Some(test_server_time()),
            last_reprice_at: None,
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state,
                working_order: Some(order),
            }],
            last_execution_reason: None,
            recovery_anomaly: None,
            stats: ExecutionStats {
                started_at: test_server_time(),
                max_inventory_gap_abs: Exposure(0.0),
                max_gap_age_ms: 0,
            },
        };
    }

    fn inventory_core_order(
        track: &poise_engine::snapshot::TrackRuntimeSnapshot,
    ) -> Option<&WorkingOrder> {
        track
            .executor_state
            .slots
            .first()
            .and_then(|slot| slot.working_order.as_ref())
    }

    fn test_snapshot_with_config(config: TrackConfig) -> TrackSnapshot {
        let mut snapshot = TrackSnapshot {
            track_id: TrackId::new("BTCUSDT"),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            config,
            status: TrackStatus::Active,
            current_exposure: Exposure(0.0),
            target_exposure: Some(Exposure(6.0)),
            manual_target_override: None,
            executor_state: ExecutorState::empty(test_server_time()),
            replacement_gate_reason: None,
            risk: RiskState::default(),
            observed: poise_engine::snapshot::ObservedState {
                reference_price: Some(95.0),
                out_of_band_since: Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap()),
                last_tick_at: None,
                market_data_stale_since: None,
            },
        };
        set_executor_state(
            &mut snapshot,
            working_order(
                Some("snapshot-1"),
                "snapshot-1",
                Side::Buy,
                94.0,
                0.25,
                Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        snapshot
    }

    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    #[derive(Clone)]
    struct MutableClock(Arc<Mutex<chrono::DateTime<Utc>>>);

    impl ClockPort for MutableClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    impl MutableClock {
        fn set(&self, value: chrono::DateTime<Utc>) {
            *self.0.lock().unwrap() = value;
        }
    }

    struct FakeExchange {
        exchange_info: ExchangeInfo,
        position: Mutex<Position>,
        open_orders: Mutex<Vec<ExchangeOrder>>,
        submitted_orders: Mutex<Vec<OrderRequest>>,
        canceled_order_ids: Mutex<Vec<String>>,
        cancel_all_symbols: Mutex<Vec<String>>,
        get_server_time_calls: AtomicUsize,
        get_position_calls: AtomicUsize,
        get_open_orders_calls: AtomicUsize,
        server_time_failures_remaining: AtomicUsize,
        position_failures_remaining: AtomicUsize,
        open_orders_failures_remaining: AtomicUsize,
        submit_error: Mutex<Option<String>>,
        cancel_order_error: Mutex<Option<String>>,
        cancel_all_error: Mutex<Option<String>>,
        server_time: chrono::DateTime<Utc>,
        sequence: AtomicUsize,
        submit_started: Option<Arc<Notify>>,
        release_submit: Option<Arc<Notify>>,
    }

    impl FakeExchange {
        fn new(position: Position, open_orders: Vec<ExchangeOrder>) -> Self {
            Self {
                exchange_info: ExchangeInfo {
                    instrument: btc_instrument(),
                    rules: ExchangeRules {
                        price_tick: 0.1,
                        quantity_step: 0.1,
                        min_qty: 0.0,
                        min_notional: 0.0,
                        maker_fee_rate: 0.0,
                        taker_fee_rate: 0.0,
                    },
                },
                position: Mutex::new(position),
                open_orders: Mutex::new(open_orders),
                submitted_orders: Mutex::new(Vec::new()),
                canceled_order_ids: Mutex::new(Vec::new()),
                cancel_all_symbols: Mutex::new(Vec::new()),
                get_server_time_calls: AtomicUsize::new(0),
                get_position_calls: AtomicUsize::new(0),
                get_open_orders_calls: AtomicUsize::new(0),
                server_time_failures_remaining: AtomicUsize::new(0),
                position_failures_remaining: AtomicUsize::new(0),
                open_orders_failures_remaining: AtomicUsize::new(0),
                submit_error: Mutex::new(None),
                cancel_order_error: Mutex::new(None),
                cancel_all_error: Mutex::new(None),
                server_time: test_server_time(),
                sequence: AtomicUsize::new(1),
                submit_started: None,
                release_submit: None,
            }
        }

        fn with_submit_error(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            error: &str,
        ) -> Self {
            let exchange = Self::new(position, open_orders);
            *exchange.submit_error.lock().unwrap() = Some(error.to_string());
            exchange
        }

        fn with_cancel_order_error(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            error: &str,
        ) -> Self {
            let exchange = Self::new(position, open_orders);
            *exchange.cancel_order_error.lock().unwrap() = Some(error.to_string());
            exchange
        }

        fn with_blocked_submit(
            position: Position,
            open_orders: Vec<ExchangeOrder>,
            submit_started: Arc<Notify>,
            release_submit: Arc<Notify>,
        ) -> Self {
            let mut exchange = Self::new(position, open_orders);
            exchange.submit_started = Some(submit_started);
            exchange.release_submit = Some(release_submit);
            exchange
        }

        fn fail_next_server_time_requests(&self, count: usize) {
            self.server_time_failures_remaining
                .store(count, Ordering::SeqCst);
        }

        fn fail_next_open_orders_requests(&self, count: usize) {
            self.open_orders_failures_remaining
                .store(count, Ordering::SeqCst);
        }

        fn set_open_orders(&self, open_orders: Vec<ExchangeOrder>) {
            *self.open_orders.lock().unwrap() = open_orders;
        }
    }

    #[async_trait::async_trait]
    impl ExchangePort for FakeExchange {
        async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
            self.submitted_orders.lock().unwrap().push(req.clone());
            if let Some(notify) = &self.submit_started {
                notify.notify_waiters();
            }
            if let Some(notify) = &self.release_submit {
                notify.notified().await;
            }
            if let Some(error) = self.submit_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            let order_id = self.sequence.fetch_add(1, Ordering::SeqCst);
            Ok(OrderReceipt {
                order_id: format!("order-{order_id}"),
                client_order_id: req.client_order_id,
                status: OrderStatus::New,
            })
        }

        async fn cancel_order(&self, _instrument: &Instrument, order_id: &str) -> Result<()> {
            self.canceled_order_ids
                .lock()
                .unwrap()
                .push(order_id.to_string());
            if let Some(error) = self.cancel_order_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            self.open_orders
                .lock()
                .unwrap()
                .retain(|order| order.order_id != order_id);
            Ok(())
        }

        async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
            self.cancel_all_symbols
                .lock()
                .unwrap()
                .push(instrument.symbol.clone());
            if let Some(error) = self.cancel_all_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            self.open_orders.lock().unwrap().clear();
            Ok(())
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            self.get_position_calls.fetch_add(1, Ordering::SeqCst);
            if self.position_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.position_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow!("temporary get_position timeout"));
            }
            Ok(self.position.lock().unwrap().clone())
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            self.get_open_orders_calls.fetch_add(1, Ordering::SeqCst);
            if self.open_orders_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.open_orders_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow!("temporary get_open_orders timeout"));
            }
            Ok(self.open_orders.lock().unwrap().clone())
        }

        async fn get_account_margin_snapshot(
            &self,
            instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountMarginSnapshot> {
            Ok(poise_engine::ports::AccountMarginSnapshot {
                venue: instrument.venue,
                available_balance: 1_000_000.0,
                total_wallet_balance: 1_000_000.0,
                max_increase_notional: 1_000_000.0,
                observed_at: Utc::now(),
            })
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(self.exchange_info.clone())
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            self.get_server_time_calls.fetch_add(1, Ordering::SeqCst);
            if self.server_time_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.server_time_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow!("temporary get_server_time timeout"));
            }
            Ok(self.server_time)
        }
    }

    #[derive(Default)]
    struct MemoryPersistence {
        snapshots: AsyncMutex<HashMap<String, TrackSnapshot>>,
        effects: AsyncMutex<Vec<PersistedTrackEffect>>,
        next_effect_batch: AtomicUsize,
        save_transition_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for MemoryPersistence {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &TrackSnapshot,
            _events: &[poise_core::events::DomainEvent],
            effects: &[ExecutionAction],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            self.save_transition_count.fetch_add(1, Ordering::SeqCst);
            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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
            apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects)
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
            let effects = self.effects.lock().await;
            Ok(effects
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }
    }

    impl MemoryPersistence {
        fn save_transition_count(&self) -> usize {
            self.save_transition_count.load(Ordering::SeqCst)
        }

        async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().await.clone()
        }

        async fn seed_effect(&self, effect: PersistedTrackEffect) {
            self.effects.lock().await.push(effect);
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for MemoryPersistence {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .cloned()
                .collect())
        }
    }

    #[derive(Default)]
    struct FailOnReceiptPersistence {
        snapshots: AsyncMutex<HashMap<String, TrackSnapshot>>,
        effects: AsyncMutex<Vec<PersistedTrackEffect>>,
        next_effect_batch: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnReceiptPersistence {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &TrackSnapshot,
            _events: &[poise_core::events::DomainEvent],
            effects: &[ExecutionAction],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            if state
                .executor_state
                .slots
                .first()
                .and_then(|slot| slot.working_order.as_ref())
                .and_then(|order| order.order_id.as_ref())
                .is_some()
            {
                return Err(anyhow!("injected receipt persistence failure"));
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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
            apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects)
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
            let effects = self.effects.lock().await;
            Ok(effects
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }
    }

    impl FailOnReceiptPersistence {
        async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for FailOnReceiptPersistence {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .cloned()
                .collect())
        }
    }

    struct FailOnSavePersistence {
        snapshots: AsyncMutex<HashMap<String, TrackSnapshot>>,
        effects: AsyncMutex<Vec<PersistedTrackEffect>>,
        next_effect_batch: AtomicUsize,
        save_count: AtomicUsize,
        fail_on: usize,
    }

    impl FailOnSavePersistence {
        fn new(fail_on: usize) -> Self {
            Self {
                snapshots: AsyncMutex::new(HashMap::new()),
                effects: AsyncMutex::new(Vec::new()),
                next_effect_batch: AtomicUsize::new(0),
                save_count: AtomicUsize::new(0),
                fail_on,
            }
        }

        async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
            self.effects.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for FailOnSavePersistence {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &TrackSnapshot,
            _events: &[poise_core::events::DomainEvent],
            effects: &[ExecutionAction],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            let save_number = self.save_count.fetch_add(1, Ordering::SeqCst) + 1;
            if save_number == self.fail_on {
                return Err(anyhow!("injected save failure"));
            }

            self.snapshots
                .lock()
                .await
                .insert(id.to_string(), state.clone());

            let now = Utc::now();
            let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
            let mut effect_store = self.effects.lock().await;
            let mut persisted_effects = Vec::new();
            for (sequence, effect) in effects.iter().enumerate() {
                if matches!(effect, ExecutionAction::NoOp) {
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
            apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(&self, id: &str) -> Result<Option<TrackSnapshot>> {
            Ok(self.snapshots.lock().await.get(id).cloned())
        }

        async fn list_track_events(
            &self,
            _id: &str,
        ) -> Result<Vec<poise_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects))
        }

        async fn list_pending_submit_effects_for_track(
            &self,
            track_id: &TrackId,
        ) -> Result<Vec<PersistedTrackEffect>> {
            let effects = self.effects.lock().await;
            Ok(ready_pending_effects(&effects)
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
            let effects = self.effects.lock().await;
            Ok(effects
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for FailOnSavePersistence {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .values()
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                })
                .collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .await
                .get(track_id.as_str())
                .cloned()
                .map(|snapshot| StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                }))
        }

        async fn list_recent_track_events(
            &self,
            _track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            _limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .await
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .cloned()
                .collect())
        }
    }

    struct FakeMarketData {
        price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
        user_receiver: Mutex<Option<mpsc::Receiver<UserDataEvent>>>,
    }

    impl FakeMarketData {
        fn new(
            price_receiver: mpsc::Receiver<PriceTick>,
            user_receiver: mpsc::Receiver<UserDataEvent>,
        ) -> Self {
            let mut price_receivers = HashMap::new();
            price_receivers.insert("BTCUSDT".to_string(), price_receiver);
            Self {
                price_receivers: Mutex::new(price_receivers),
                user_receiver: Mutex::new(Some(user_receiver)),
            }
        }

        fn without_user_receiver(price_receiver: mpsc::Receiver<PriceTick>) -> Self {
            let mut price_receivers = HashMap::new();
            price_receivers.insert("BTCUSDT".to_string(), price_receiver);
            Self {
                price_receivers: Mutex::new(price_receivers),
                user_receiver: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl MarketDataPort for FakeMarketData {
        async fn subscribe_prices(
            &self,
            instrument: &Instrument,
        ) -> Result<mpsc::Receiver<PriceTick>> {
            self.price_receivers
                .lock()
                .unwrap()
                .remove(&instrument.symbol)
                .ok_or_else(|| anyhow!("missing test price receiver for {}", instrument.symbol))
        }

        async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
            let receiver = self
                .user_receiver
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow!("missing test user receiver"))?;

            Ok(receiver)
        }
    }
}
