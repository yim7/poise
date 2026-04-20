use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use poise_application::ApplicationNotification;
use poise_protocol::{
    PriceExecutionBlockReasonView, StreamEvent, TrackLiveView as ProtocolTrackLiveView,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::server_context::WebSocketState;

#[cfg(not(test))]
const WEBSOCKET_DIAGNOSTIC_LOG_INTERVAL: Duration = Duration::from_secs(5);
#[cfg(test)]
const WEBSOCKET_DIAGNOSTIC_LOG_INTERVAL: Duration = Duration::from_millis(100);

static NEXT_WEBSOCKET_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
const LIVE_VIEW_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

pub async fn ws_handler(ws: WebSocketUpgrade, state: WebSocketState) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WebSocketDiagnosticsSnapshot {
    connection_id: u64,
    window_duration: Duration,
    raw_track_notifications: usize,
    raw_account_notifications: usize,
    raw_live_notifications: usize,
    batches: usize,
    max_batch_size: usize,
    track_pushes: usize,
    account_pushes: usize,
    live_pushes: usize,
    live_flushes: usize,
    avg_tracks_per_live_flush: f64,
    max_tracks_per_live_flush: usize,
    live_coalesced: usize,
    detail_query_count: usize,
    avg_detail_query: Duration,
    max_detail_query: Duration,
    live_query_count: usize,
    avg_live_query: Duration,
    max_live_query: Duration,
    send_count: usize,
    avg_send: Duration,
    max_send: Duration,
}

struct WebSocketDiagnostics {
    connection_id: u64,
    window_started_at: Instant,
    raw_track_notifications: usize,
    raw_account_notifications: usize,
    raw_live_notifications: usize,
    batches: usize,
    max_batch_size: usize,
    track_pushes: usize,
    account_pushes: usize,
    live_pushes: usize,
    live_flushes: usize,
    total_tracks_per_live_flush: usize,
    max_tracks_per_live_flush: usize,
    detail_query_count: usize,
    total_detail_query: Duration,
    max_detail_query: Duration,
    live_query_count: usize,
    total_live_query: Duration,
    max_live_query: Duration,
    send_count: usize,
    total_send: Duration,
    max_send: Duration,
}

impl WebSocketDiagnostics {
    fn new(connection_id: u64, started_at: Instant) -> Self {
        Self {
            connection_id,
            window_started_at: started_at,
            raw_track_notifications: 0,
            raw_account_notifications: 0,
            raw_live_notifications: 0,
            batches: 0,
            max_batch_size: 0,
            track_pushes: 0,
            account_pushes: 0,
            live_pushes: 0,
            live_flushes: 0,
            total_tracks_per_live_flush: 0,
            max_tracks_per_live_flush: 0,
            detail_query_count: 0,
            total_detail_query: Duration::ZERO,
            max_detail_query: Duration::ZERO,
            live_query_count: 0,
            total_live_query: Duration::ZERO,
            max_live_query: Duration::ZERO,
            send_count: 0,
            total_send: Duration::ZERO,
            max_send: Duration::ZERO,
        }
    }

    fn record_notification(&mut self, notification: &ApplicationNotification) {
        match notification {
            ApplicationNotification::TrackChanged { .. } => {
                self.raw_track_notifications += 1;
            }
            ApplicationNotification::AccountChanged => {
                self.raw_account_notifications += 1;
            }
        }
    }

    fn record_live_notification(&mut self) {
        self.raw_live_notifications += 1;
    }

    fn record_batch(&mut self, batch_size: usize) {
        if batch_size == 0 {
            return;
        }
        self.batches += 1;
        self.max_batch_size = self.max_batch_size.max(batch_size);
    }

    fn record_track_push(&mut self) {
        self.track_pushes += 1;
    }

    fn record_account_push(&mut self) {
        self.account_pushes += 1;
    }

    fn record_live_push(&mut self) {
        self.live_pushes += 1;
    }

    fn record_live_flush(&mut self, track_count: usize) {
        if track_count == 0 {
            return;
        }
        self.live_flushes += 1;
        self.total_tracks_per_live_flush += track_count;
        self.max_tracks_per_live_flush = self.max_tracks_per_live_flush.max(track_count);
    }

    fn record_detail_query(&mut self, elapsed: Duration) {
        self.detail_query_count += 1;
        self.total_detail_query += elapsed;
        self.max_detail_query = self.max_detail_query.max(elapsed);
    }

    fn record_live_query(&mut self, elapsed: Duration) {
        self.live_query_count += 1;
        self.total_live_query += elapsed;
        self.max_live_query = self.max_live_query.max(elapsed);
    }

    fn record_send(&mut self, elapsed: Duration) {
        self.send_count += 1;
        self.total_send += elapsed;
        self.max_send = self.max_send.max(elapsed);
    }

    fn snapshot(&self, now: Instant) -> WebSocketDiagnosticsSnapshot {
        WebSocketDiagnosticsSnapshot {
            connection_id: self.connection_id,
            window_duration: now.duration_since(self.window_started_at),
            raw_track_notifications: self.raw_track_notifications,
            raw_account_notifications: self.raw_account_notifications,
            raw_live_notifications: self.raw_live_notifications,
            batches: self.batches,
            max_batch_size: self.max_batch_size,
            track_pushes: self.track_pushes,
            account_pushes: self.account_pushes,
            live_pushes: self.live_pushes,
            live_flushes: self.live_flushes,
            avg_tracks_per_live_flush: average_count(
                self.total_tracks_per_live_flush,
                self.live_flushes,
            ),
            max_tracks_per_live_flush: self.max_tracks_per_live_flush,
            live_coalesced: self.raw_live_notifications.saturating_sub(self.live_pushes),
            detail_query_count: self.detail_query_count,
            avg_detail_query: average_duration(self.total_detail_query, self.detail_query_count),
            max_detail_query: self.max_detail_query,
            live_query_count: self.live_query_count,
            avg_live_query: average_duration(self.total_live_query, self.live_query_count),
            max_live_query: self.max_live_query,
            send_count: self.send_count,
            avg_send: average_duration(self.total_send, self.send_count),
            max_send: self.max_send,
        }
    }

    fn take_due_snapshot(&mut self, now: Instant) -> Option<WebSocketDiagnosticsSnapshot> {
        if now.duration_since(self.window_started_at) < WEBSOCKET_DIAGNOSTIC_LOG_INTERVAL {
            return None;
        }
        let snapshot = self.snapshot(now);
        self.reset(now);
        Some(snapshot)
    }

    fn take_snapshot_and_reset(&mut self, now: Instant) -> WebSocketDiagnosticsSnapshot {
        let snapshot = self.snapshot(now);
        self.reset(now);
        snapshot
    }

    fn reset(&mut self, now: Instant) {
        self.window_started_at = now;
        self.raw_track_notifications = 0;
        self.raw_account_notifications = 0;
        self.raw_live_notifications = 0;
        self.batches = 0;
        self.max_batch_size = 0;
        self.track_pushes = 0;
        self.account_pushes = 0;
        self.live_pushes = 0;
        self.live_flushes = 0;
        self.total_tracks_per_live_flush = 0;
        self.max_tracks_per_live_flush = 0;
        self.detail_query_count = 0;
        self.total_detail_query = Duration::ZERO;
        self.max_detail_query = Duration::ZERO;
        self.live_query_count = 0;
        self.total_live_query = Duration::ZERO;
        self.max_live_query = Duration::ZERO;
        self.send_count = 0;
        self.total_send = Duration::ZERO;
        self.max_send = Duration::ZERO;
    }
}

fn log_websocket_diagnostics(snapshot: &WebSocketDiagnosticsSnapshot) {
    tracing::info!(
        connection_id = snapshot.connection_id,
        window_ms = snapshot.window_duration.as_millis() as u64,
        raw_track_notifications = snapshot.raw_track_notifications,
        raw_account_notifications = snapshot.raw_account_notifications,
        raw_live_notifications = snapshot.raw_live_notifications,
        batches = snapshot.batches,
        max_batch_size = snapshot.max_batch_size,
        track_pushes = snapshot.track_pushes,
        account_pushes = snapshot.account_pushes,
        live_pushes = snapshot.live_pushes,
        live_flushes = snapshot.live_flushes,
        avg_tracks_per_live_flush = snapshot.avg_tracks_per_live_flush,
        max_tracks_per_live_flush = snapshot.max_tracks_per_live_flush,
        live_coalesced = snapshot.live_coalesced,
        detail_query_count = snapshot.detail_query_count,
        avg_detail_query_ms = snapshot.avg_detail_query.as_secs_f64() * 1000.0,
        max_detail_query_ms = snapshot.max_detail_query.as_secs_f64() * 1000.0,
        live_query_count = snapshot.live_query_count,
        avg_live_query_ms = snapshot.avg_live_query.as_secs_f64() * 1000.0,
        max_live_query_ms = snapshot.max_live_query.as_secs_f64() * 1000.0,
        send_count = snapshot.send_count,
        avg_send_ms = snapshot.avg_send.as_secs_f64() * 1000.0,
        max_send_ms = snapshot.max_send.as_secs_f64() * 1000.0,
        "websocket push diagnostics"
    );
}

fn log_websocket_lag(snapshot: &WebSocketDiagnosticsSnapshot, skipped: u64) {
    tracing::warn!(
        connection_id = snapshot.connection_id,
        skipped,
        window_ms = snapshot.window_duration.as_millis() as u64,
        raw_track_notifications = snapshot.raw_track_notifications,
        raw_account_notifications = snapshot.raw_account_notifications,
        raw_live_notifications = snapshot.raw_live_notifications,
        batches = snapshot.batches,
        max_batch_size = snapshot.max_batch_size,
        track_pushes = snapshot.track_pushes,
        account_pushes = snapshot.account_pushes,
        live_pushes = snapshot.live_pushes,
        live_flushes = snapshot.live_flushes,
        avg_tracks_per_live_flush = snapshot.avg_tracks_per_live_flush,
        max_tracks_per_live_flush = snapshot.max_tracks_per_live_flush,
        live_coalesced = snapshot.live_coalesced,
        detail_query_count = snapshot.detail_query_count,
        avg_detail_query_ms = snapshot.avg_detail_query.as_secs_f64() * 1000.0,
        max_detail_query_ms = snapshot.max_detail_query.as_secs_f64() * 1000.0,
        live_query_count = snapshot.live_query_count,
        avg_live_query_ms = snapshot.avg_live_query.as_secs_f64() * 1000.0,
        max_live_query_ms = snapshot.max_live_query.as_secs_f64() * 1000.0,
        send_count = snapshot.send_count,
        avg_send_ms = snapshot.avg_send.as_secs_f64() * 1000.0,
        max_send_ms = snapshot.max_send.as_secs_f64() * 1000.0,
        "websocket notification stream lagged; closing socket for resync"
    );
}

#[cfg(test)]
fn emit_test_snapshot(state: &WebSocketState, snapshot: WebSocketDiagnosticsSnapshot) {
    if let Some(tx) = &state.diagnostics_tx {
        let _ = tx.send(snapshot);
    }
}

#[cfg(not(test))]
fn emit_test_snapshot(_state: &WebSocketState, _snapshot: WebSocketDiagnosticsSnapshot) {}

fn average_duration(total: Duration, count: usize) -> Duration {
    if count == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(total.as_secs_f64() / count as f64)
}

fn average_count(total: usize, count: usize) -> f64 {
    if count == 0 {
        return 0.0;
    }
    total as f64 / count as f64
}

enum PendingSocketUpdate {
    TrackChanged {
        track_id: poise_engine::track::TrackId,
    },
    AccountChanged,
}

#[derive(Default)]
struct PendingSocketUpdates {
    track_ids: Vec<poise_engine::track::TrackId>,
    pending_track_ids: HashSet<poise_engine::track::TrackId>,
    account_changed: bool,
}

#[derive(Default)]
struct PendingLiveViewUpdates {
    track_ids: Vec<String>,
    pending_track_ids: HashSet<String>,
}

impl PendingLiveViewUpdates {
    fn record(&mut self, track_id: String) {
        if self.pending_track_ids.insert(track_id.clone()) {
            self.track_ids.push(track_id);
        }
    }

    fn take(&mut self) -> Vec<String> {
        let track_ids = self.track_ids.drain(..).collect();
        self.pending_track_ids.clear();
        track_ids
    }
}

impl PendingSocketUpdates {
    fn is_empty(&self) -> bool {
        self.track_ids.is_empty() && !self.account_changed
    }

    fn record(&mut self, notification: ApplicationNotification) {
        match notification {
            ApplicationNotification::TrackChanged { track_id } => {
                if self.pending_track_ids.insert(track_id.clone()) {
                    self.track_ids.push(track_id);
                }
            }
            ApplicationNotification::AccountChanged => {
                self.account_changed = true;
            }
        }
    }

    fn take(&mut self) -> Vec<PendingSocketUpdate> {
        let mut updates =
            Vec::with_capacity(self.track_ids.len() + usize::from(self.account_changed));
        updates.extend(
            self.track_ids
                .drain(..)
                .map(|track_id| PendingSocketUpdate::TrackChanged { track_id }),
        );
        self.pending_track_ids.clear();
        if self.account_changed {
            updates.push(PendingSocketUpdate::AccountChanged);
            self.account_changed = false;
        }
        updates
    }
}

#[cfg(test)]
pub async fn ws_handler_with_test_state(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<WebSocketState>,
) -> Response {
    ws_handler(ws, state).await
}

async fn handle_socket(mut socket: WebSocket, state: WebSocketState) {
    let mut receiver = state.notifications.subscribe();
    let mut live_receiver = state.live_view_notifications.subscribe();
    let mut pending = PendingSocketUpdates::default();
    let mut pending_live = PendingLiveViewUpdates::default();
    let mut live_flush_deadline: Option<tokio::time::Instant> = None;
    let connection_id = NEXT_WEBSOCKET_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let mut diagnostics = WebSocketDiagnostics::new(connection_id, Instant::now());

    loop {
        let mut batch_size = 0;
        if pending.is_empty() {
            let flush_sleep = live_flush_deadline.map(tokio::time::sleep_until);
            tokio::pin!(flush_sleep);

            tokio::select! {
                biased;
                result = receiver.recv() => {
                    match result {
                        Ok(notification) => {
                            diagnostics.record_notification(&notification);
                            pending.record(notification);
                            batch_size += 1;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            let snapshot = diagnostics.take_snapshot_and_reset(Instant::now());
                            log_websocket_lag(&snapshot, skipped as u64);
                            emit_test_snapshot(&state, snapshot);
                            close_socket(&mut socket).await;
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                result = live_receiver.recv() => {
                    match result {
                        Ok(track_id) => {
                            diagnostics.record_live_notification();
                            pending_live.record(track_id);
                            if live_flush_deadline.is_none() {
                                live_flush_deadline = Some(
                                    tokio::time::Instant::now() + LIVE_VIEW_FLUSH_INTERVAL
                                );
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            let snapshot = diagnostics.take_snapshot_and_reset(Instant::now());
                            log_websocket_lag(&snapshot, skipped as u64);
                            emit_test_snapshot(&state, snapshot);
                            close_socket(&mut socket).await;
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = async { if let Some(sleep) = flush_sleep.as_mut().as_pin_mut() { sleep.await } }, if live_flush_deadline.is_some() => {}
            }
        }

        loop {
            match receiver.try_recv() {
                Ok(notification) => {
                    diagnostics.record_notification(&notification);
                    pending.record(notification);
                    batch_size += 1;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    let snapshot = diagnostics.take_snapshot_and_reset(Instant::now());
                    log_websocket_lag(&snapshot, skipped as u64);
                    emit_test_snapshot(&state, snapshot);
                    close_socket(&mut socket).await;
                    return;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
            }
        }
        diagnostics.record_batch(batch_size);

        loop {
            match live_receiver.try_recv() {
                Ok(track_id) => {
                    diagnostics.record_live_notification();
                    pending_live.record(track_id);
                    if live_flush_deadline.is_none() {
                        live_flush_deadline =
                            Some(tokio::time::Instant::now() + LIVE_VIEW_FLUSH_INTERVAL);
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    let snapshot = diagnostics.take_snapshot_and_reset(Instant::now());
                    log_websocket_lag(&snapshot, skipped as u64);
                    emit_test_snapshot(&state, snapshot);
                    close_socket(&mut socket).await;
                    return;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
            }
        }

        for update in pending.take() {
            let pushed = match update {
                PendingSocketUpdate::TrackChanged { track_id } => {
                    diagnostics.record_track_push();
                    push_projected_updates(&mut socket, &state, track_id, &mut diagnostics).await
                }
                PendingSocketUpdate::AccountChanged => {
                    diagnostics.record_account_push();
                    push_account_summary(&mut socket, &state, &mut diagnostics).await
                }
            };
            if !pushed {
                return;
            }
        }

        if live_flush_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            let track_ids = pending_live.take();
            diagnostics.record_live_flush(track_ids.len());
            for track_id in track_ids {
                diagnostics.record_live_push();
                if !push_live_view_update(&mut socket, &state, &track_id, &mut diagnostics).await {
                    return;
                }
            }
            live_flush_deadline = None;
        }

        if let Some(snapshot) = diagnostics.take_due_snapshot(Instant::now()) {
            log_websocket_diagnostics(&snapshot);
            emit_test_snapshot(&state, snapshot);
        }
    }
}

async fn push_projected_updates(
    socket: &mut WebSocket,
    state: &WebSocketState,
    track_id: poise_engine::track::TrackId,
    diagnostics: &mut WebSocketDiagnostics,
) -> bool {
    let load_started_at = Instant::now();
    let source = match state
        .query_service
        .load_track_detail_source(&track_id)
        .await
    {
        Ok(Some(source)) => {
            diagnostics.record_detail_query(load_started_at.elapsed());
            source
        }
        Ok(None) => {
            diagnostics.record_detail_query(load_started_at.elapsed());
            tracing::warn!(
                "track `{}` missing from read model during websocket push; closing socket for resync",
                track_id.as_str()
            );
            close_socket(socket).await;
            return false;
        }
        Err(error) => {
            diagnostics.record_detail_query(load_started_at.elapsed());
            tracing::warn!(
                "failed to load read model for websocket track `{}`: {error}; closing socket for resync",
                track_id.as_str()
            );
            close_socket(socket).await;
            return false;
        }
    };

    let track_id_text = track_id.as_str().to_string();
    let list_item = state.projector.project_list_item(&source);
    let detail = state.projector.project_detail(&source);
    let events = [
        StreamEvent::TrackListItemChanged {
            track_id: track_id_text.clone(),
            item: list_item,
        },
        StreamEvent::TrackDetailChanged {
            track_id: track_id_text,
            detail: Box::new(detail),
        },
    ];

    for event in events {
        if !send_event(socket, event, diagnostics).await {
            return false;
        }
    }

    true
}

async fn push_live_view_update(
    socket: &mut WebSocket,
    state: &WebSocketState,
    track_id: &str,
    diagnostics: &mut WebSocketDiagnostics,
) -> bool {
    let query_started_at = Instant::now();
    let live_view = match state.observation_service.track_live_view(track_id).await {
        Ok(live_view) => {
            diagnostics.record_live_query(query_started_at.elapsed());
            live_view
        }
        Err(error) => {
            diagnostics.record_live_query(query_started_at.elapsed());
            tracing::warn!(
                "failed to load live view for websocket track `{track_id}`: {error}; closing socket for resync"
            );
            close_socket(socket).await;
            return false;
        }
    };

    send_event(
        socket,
        StreamEvent::TrackLiveViewChanged {
            track_id: track_id.to_string(),
            live: project_live_view(live_view),
        },
        diagnostics,
    )
    .await
}

fn project_live_view(live_view: poise_engine::runtime::TrackLiveView) -> ProtocolTrackLiveView {
    ProtocolTrackLiveView {
        strategy_price: live_view.strategy_price,
        strategy_price_status: match live_view.strategy_price_status {
            poise_engine::runtime::StrategyPriceStatus::Live => {
                poise_protocol::StrategyPriceStatusView::Live
            }
            poise_engine::runtime::StrategyPriceStatus::Stale => {
                poise_protocol::StrategyPriceStatusView::Stale
            }
        },
        mark_price: live_view.mark_price,
        best_bid: live_view.best_bid,
        best_ask: live_view.best_ask,
        desired_exposure: live_view.desired_exposure,
        price_execution_block_reason: live_view
            .price_execution_block_reason
            .map(project_price_execution_block_reason),
    }
}

fn project_price_execution_block_reason(
    reason: poise_engine::price_gate::PriceExecutionBlockReason,
) -> PriceExecutionBlockReasonView {
    match reason {
        poise_engine::price_gate::PriceExecutionBlockReason::MissingExecutionQuote => {
            PriceExecutionBlockReasonView::MissingExecutionQuote
        }
        poise_engine::price_gate::PriceExecutionBlockReason::MarkBookDivergence => {
            PriceExecutionBlockReasonView::MarkBookDivergence
        }
    }
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

async fn push_account_summary(
    socket: &mut WebSocket,
    state: &WebSocketState,
    diagnostics: &mut WebSocketDiagnostics,
) -> bool {
    let Some(summary) = state.account_monitor.current_summary().await else {
        return true;
    };
    send_event(
        socket,
        StreamEvent::AccountSummaryChanged {
            summary: state.account_projector.project_summary(&summary),
        },
        diagnostics,
    )
    .await
}

async fn send_event(
    socket: &mut WebSocket,
    event: StreamEvent,
    diagnostics: &mut WebSocketDiagnostics,
) -> bool {
    let message = match serde_json::to_string(&event) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!("failed to serialize websocket event: {error}");
            return true;
        }
    };

    let send_started_at = Instant::now();
    let sent = socket.send(Message::Text(message)).await.is_ok();
    diagnostics.record_send(send_started_at.elapsed());
    sent
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use axum::Router;
    use chrono::{TimeZone, Utc};
    use futures_util::{SinkExt, StreamExt};
    use poise_application::{
        CommittedTrackWrite, EffectStatus, EffectStatusUpdate, FollowUpRetirementRequest,
        PersistedTrackEffect, StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore,
        TrackMutationStore, TrackQueryStore,
    };
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use poise_engine::command::TrackCommand;
    use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{AccountSummarySnapshot, ClockPort};
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_protocol::{
        ExecutionStateView, ExecutionStatusView, RiskSignalView, StreamEvent, TrackStatus,
    };
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::effect_worker::EffectWorker;
    use crate::projector::TrackProjector;
    use crate::server_context::WebSocketState;
    use crate::test_support::{
        build_effect_worker_test_context, build_test_application_services, build_websocket_state,
        test_prepared_registry, unavailable_account_monitor,
    };
    use poise_application::{
        AccountMonitor, AccountMonitorConfig, AccountMonitorStore, ApplicationNotification,
        StoredAccountMonitorState, TrackCommandService, TrackQueryService,
    };

    use super::{WebSocketDiagnostics, WebSocketDiagnosticsSnapshot, ws_handler_with_test_state};

    #[derive(Clone)]
    struct WebSocketTestContext {
        websocket_state: WebSocketState,
        command_service: Arc<TrackCommandService>,
        notifications: tokio::sync::broadcast::Sender<ApplicationNotification>,
    }

    type ClientStream = futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >;

    async fn spawn_server(repository: Arc<TestRepository>) -> (String, WebSocketTestContext) {
        spawn_server_with_capacity(repository, 16).await
    }

    #[tokio::test]
    async fn websocket_accepts_websocket_state_without_effect_worker_dependencies() {
        let repository = Arc::new(TestRepository::default());
        let (_url, state) = spawn_server(repository).await;
        let websocket_state = state.websocket_state.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/ws",
            axum::routing::get(move |ws| super::ws_handler(ws, websocket_state.clone())),
        );

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (client, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let (mut sink, _) = client.split();
        sink.close().await.unwrap();
    }

    async fn spawn_server_with_capacity(
        repository: Arc<TestRepository>,
        notification_capacity: usize,
    ) -> (String, WebSocketTestContext) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(notification_capacity);
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectStore>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            mutation_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new_with_observation(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_prepared_registry("btc-core"),
            Some(Arc::clone(&services.observation_service)),
        ));
        let websocket_state = build_websocket_state(
            &services,
            query_service,
            Arc::new(TrackProjector::new()),
            unavailable_account_monitor(services.notifications.clone()),
            Arc::new(crate::account_projector::AccountProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler_with_test_state))
            .with_state(websocket_state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            format!("ws://{address}/ws"),
            WebSocketTestContext {
                websocket_state,
                command_service: services.command_service.clone(),
                notifications: services.notifications.clone(),
            },
        )
    }

    async fn spawn_server_with_account_monitor(
        repository: Arc<TestRepository>,
        account_monitor: Arc<AccountMonitor>,
    ) -> (String, WebSocketTestContext) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectStore>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            mutation_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new_with_observation(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_prepared_registry("btc-core"),
            Some(Arc::clone(&services.observation_service)),
        ));
        let websocket_state = build_websocket_state(
            &services,
            query_service,
            Arc::new(TrackProjector::new()),
            account_monitor,
            Arc::new(crate::account_projector::AccountProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler_with_test_state))
            .with_state(websocket_state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            format!("ws://{address}/ws"),
            WebSocketTestContext {
                websocket_state,
                command_service: services.command_service.clone(),
                notifications: services.notifications.clone(),
            },
        )
    }

    async fn spawn_server_with_diagnostics(
        repository: Arc<TestRepository>,
        notification_capacity: usize,
    ) -> (
        String,
        WebSocketTestContext,
        tokio::sync::mpsc::UnboundedReceiver<WebSocketDiagnosticsSnapshot>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(notification_capacity);
        let (diagnostics_tx, diagnostics_rx) = tokio::sync::mpsc::unbounded_channel();
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository.clone() as Arc<dyn TrackEffectStore>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            mutation_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard.clone(),
        );
        let query_service = Arc::new(TrackQueryService::new_with_observation(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_prepared_registry("btc-core"),
            Some(Arc::clone(&services.observation_service)),
        ));
        let mut websocket_state = build_websocket_state(
            &services,
            query_service,
            Arc::new(TrackProjector::new()),
            unavailable_account_monitor(services.notifications.clone()),
            Arc::new(crate::account_projector::AccountProjector::new()),
        );
        websocket_state.diagnostics_tx = Some(diagnostics_tx);
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler_with_test_state))
            .with_state(websocket_state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (
            format!("ws://{address}/ws"),
            WebSocketTestContext {
                websocket_state,
                command_service: services.command_service.clone(),
                notifications: services.notifications.clone(),
            },
            diagnostics_rx,
        )
    }

    fn build_effect_worker_state_for_notification_test(
        repository: Arc<TestRepository>,
        notifications: tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) -> crate::server_context::EffectWorkerState {
        let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
        let effect_store = repository as Arc<dyn TrackEffectStore>;
        let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
        let services = build_test_application_services(
            test_manager(),
            mutation_store.clone(),
            effect_store.clone(),
            notifications,
            account_margin_guard,
        );

        build_effect_worker_test_context(&services, mutation_store, effect_store)
            .effect_worker_state
    }

    async fn recv_event(stream: &mut ClientStream) -> StreamEvent {
        let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    fn seeded_repository() -> Arc<TestRepository> {
        let repository = Arc::new(TestRepository::default());
        let mut snapshot = test_manager().snapshot("btc-core").unwrap();
        seed_snapshot_ledger(&mut snapshot);
        repository.seed_snapshot(snapshot);
        repository
    }

    async fn seeded_account_monitor(
        notifications: tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) -> Arc<AccountMonitor> {
        let account_store: Arc<dyn AccountMonitorStore> =
            Arc::new(poise_storage::sqlite::SqliteStorage::in_memory().unwrap());
        account_store
            .save_state(&StoredAccountMonitorState {
                trading_day: chrono::NaiveDate::from_ymd_opt(2026, 4, 4).unwrap(),
                baseline_equity: 13_000.0,
                baseline_captured_at: Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap(),
                last_observed_account_snapshot: Some(AccountSummarySnapshot {
                    equity: 12_500.0,
                    available: 9_000.0,
                    unrealized_pnl: -350.0,
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
                }),
            })
            .await
            .unwrap();

        Arc::new(
            AccountMonitor::restore(
                Arc::new(NoopExchange),
                account_store,
                notifications,
                AccountMonitorConfig::default(),
            )
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn broadcasts_events_to_multiple_clients() {
        let repository = seeded_repository();
        let (url, state) = spawn_server(repository).await;
        let (client_a, _) = connect_async(&url).await.unwrap();
        let (client_b, _) = connect_async(&url).await.unwrap();
        let (_, mut stream_a) = client_a.split();
        let (_, mut stream_b) = client_b.split();

        let _ = state
            .notifications
            .send(ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            });

        let payload_a = recv_event(&mut stream_a).await;
        let payload_b = recv_event(&mut stream_b).await;

        assert_eq!(payload_a, payload_b);
        assert!(matches!(
            payload_a,
            StreamEvent::TrackListItemChanged { ref track_id, .. } if track_id == "btc-core"
        ));
    }

    #[tokio::test]
    async fn broadcasts_track_events_with_stream_event_envelope() {
        let repository = seeded_repository();
        let (url, state) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ =
            state
                .notifications
                .send(poise_application::ApplicationNotification::TrackChanged {
                    track_id: TrackId::new("btc-core"),
                });

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TrackListItemChanged { track_id, .. } if track_id == "btc-core"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TrackDetailChanged { track_id, .. } if track_id == "btc-core"
        )));
    }

    #[tokio::test]
    async fn broadcasts_account_summary_changed_after_account_notification() {
        let repository = seeded_repository();
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let account_monitor = seeded_account_monitor(notifications.clone()).await;
        let (url, state) = spawn_server_with_account_monitor(repository, account_monitor).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ = state
            .notifications
            .send(ApplicationNotification::AccountChanged);

        let event = recv_event(&mut stream).await;

        match event {
            StreamEvent::AccountSummaryChanged { summary } => {
                assert_eq!(summary.equity, Some(12_500.0));
                assert_eq!(summary.available, Some(9_000.0));
                assert_eq!(summary.unrealized_pnl, Some(-350.0));
                assert_eq!(summary.risk_signal, RiskSignalView::Attention);
                assert_eq!(summary.reason.as_deref(), Some("day_change -3.8%"));
            }
            other => panic!("expected account summary event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn broadcasts_track_detail_changed_after_write_commit() {
        let repository = seeded_repository();
        let (url, state) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        state
            .command_service
            .command("btc-core", TrackCommand::Pause)
            .await
            .unwrap();

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TrackListItemChanged { track_id, .. } if track_id == "btc-core"
        )));
        let detail = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::TrackDetailChanged { detail, .. } => Some(detail),
                _ => None,
            })
            .expect("should emit projected detail change");
        let detail_json = serde_json::to_value(detail).unwrap();
        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.status.lifecycle.status, TrackStatus::Paused);
        assert_eq!(detail.execution.state, ExecutionStateView::Paused);
        assert_eq!(
            detail_json["ledger"]["total_pnl"].as_f64(),
            Some(detail.ledger.total_pnl)
        );
        assert_eq!(
            detail_json["ledger"]["unrealized_pnl"].as_f64(),
            Some(detail.ledger.unrealized_pnl)
        );
        assert_eq!(
            detail_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
            Some(detail.execution_stats.max_inventory_gap_abs)
        );
    }

    #[tokio::test]
    async fn broadcasts_track_list_item_changed_after_effect_state_change() {
        let repository = seeded_repository();
        repository.seed_pending_noop_effect();
        let (url, state) = spawn_server(repository.clone()).await;
        let effect_worker_state = build_effect_worker_state_for_notification_test(
            repository,
            state.notifications.clone(),
        );
        let worker = EffectWorker::new(
            effect_worker_state,
            Arc::new(NoopExchange),
            Arc::new(NoopExchange),
            Duration::from_millis(10),
        );
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        worker.run_once().await.unwrap();

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        let item = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::TrackListItemChanged { item, .. } => Some(item),
                _ => None,
            })
            .expect("should emit projected list item change");
        let item_json = serde_json::to_value(item).unwrap();
        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_slot_count, 1);
        assert_eq!(
            item_json["ledger"]["total_pnl"].as_f64(),
            Some(item.ledger.total_pnl)
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::TrackDetailChanged { .. }))
        );
    }

    #[tokio::test]
    async fn coalesces_duplicate_track_notifications_before_loading_read_model() {
        let repository = seeded_repository();
        repository.set_read_delay(Duration::from_millis(10));
        let (url, state) = spawn_server_with_capacity(repository.clone(), 16).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..5 {
            let _ = state
                .notifications
                .send(ApplicationNotification::TrackChanged {
                    track_id: TrackId::new("btc-core"),
                });
        }

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TrackListItemChanged { track_id, .. } if track_id == "btc-core"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::TrackDetailChanged { track_id, .. } if track_id == "btc-core"
        )));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(repository.load_snapshot_calls(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err(),
            "duplicate track notifications should not emit extra websocket events"
        );
    }

    #[tokio::test]
    async fn websocket_coalesces_live_view_updates_per_track_at_250ms_windows() {
        let repository = seeded_repository();
        let (url, state) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..8 {
            let _ = state
                .websocket_state
                .live_view_notifications
                .send("btc-core".to_string());
        }

        let event = tokio::time::timeout(Duration::from_secs(1), recv_event(&mut stream))
            .await
            .expect("live view change should flush within debounce window");
        assert!(matches!(
            event,
            StreamEvent::TrackLiveViewChanged { ref track_id, .. } if track_id == "btc-core"
        ));

        assert!(
            tokio::time::timeout(Duration::from_millis(150), stream.next())
                .await
                .is_err(),
            "same-track live notifications in one window should coalesce into a single push"
        );
    }

    #[tokio::test]
    async fn websocket_live_view_updates_do_not_trigger_full_detail_projection() {
        let repository = seeded_repository();
        let (url, state) = spawn_server(repository.clone()).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ = state
            .websocket_state
            .live_view_notifications
            .send("btc-core".to_string());

        let event = tokio::time::timeout(Duration::from_secs(1), recv_event(&mut stream))
            .await
            .expect("live view change should flush");
        assert!(matches!(
            event,
            StreamEvent::TrackLiveViewChanged { ref track_id, .. } if track_id == "btc-core"
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(repository.load_snapshot_calls(), 0);
    }

    #[tokio::test]
    async fn closes_socket_when_notification_stream_lags() {
        let repository = seeded_repository();
        repository.set_read_delay(Duration::from_millis(50));
        let (url, state) = spawn_server_with_capacity(repository, 1).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..8 {
            let _ = state
                .notifications
                .send(ApplicationNotification::TrackChanged {
                    track_id: TrackId::new("btc-core"),
                });
        }

        let next = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.next().await {
                    None => return None,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                        return Some(frame);
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
                    | Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_))) => continue,
                    Some(other) => {
                        panic!("unexpected websocket message after lagged stream: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("lagged websocket should close instead of hanging");
        assert!(matches!(next, None | Some(_)));
    }

    #[tokio::test]
    async fn closes_socket_when_track_read_model_is_missing_for_notification() {
        let repository = seeded_repository();
        repository.remove_snapshot("btc-core");
        let (url, state) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ = state
            .notifications
            .send(ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            });

        let next = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.next().await {
                    None => return None,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                        return Some(frame);
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
                    | Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_))) => continue,
                    Some(other) => {
                        panic!("unexpected websocket message after missing read model: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("missing read model should close websocket for resync");
        assert!(matches!(next, None | Some(_)));
    }

    #[tokio::test]
    async fn closes_socket_when_track_read_model_load_fails() {
        let repository = seeded_repository();
        repository.set_load_snapshot_error("injected read failure");
        let (url, state) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ = state
            .notifications
            .send(ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            });

        let next = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match stream.next().await {
                    None => return None,
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(frame))) => {
                        return Some(frame);
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
                    | Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_))) => continue,
                    Some(other) => {
                        panic!("unexpected websocket message after read failure: {other:?}")
                    }
                }
            }
        })
        .await
        .expect("read model failure should close websocket for resync");
        assert!(matches!(next, None | Some(_)));
    }

    #[test]
    fn websocket_diagnostics_snapshot_tracks_counts_and_timings() {
        let started_at = std::time::Instant::now();
        let mut diagnostics = WebSocketDiagnostics::new(7, started_at);

        diagnostics.record_notification(&ApplicationNotification::TrackChanged {
            track_id: TrackId::new("btc-core"),
        });
        diagnostics.record_notification(&ApplicationNotification::TrackChanged {
            track_id: TrackId::new("eth-core"),
        });
        diagnostics.record_notification(&ApplicationNotification::AccountChanged);
        diagnostics.record_live_notification();
        diagnostics.record_live_notification();
        diagnostics.record_live_notification();
        diagnostics.record_live_notification();
        diagnostics.record_live_notification();
        diagnostics.record_batch(3);
        diagnostics.record_track_push();
        diagnostics.record_track_push();
        diagnostics.record_account_push();
        diagnostics.record_live_push();
        diagnostics.record_live_push();
        diagnostics.record_live_push();
        diagnostics.record_live_flush(2);
        diagnostics.record_live_flush(1);
        diagnostics.record_detail_query(Duration::from_millis(12));
        diagnostics.record_detail_query(Duration::from_millis(18));
        diagnostics.record_send(Duration::from_millis(4));
        diagnostics.record_send(Duration::from_millis(10));

        let snapshot = diagnostics.snapshot(started_at + Duration::from_secs(5));

        assert_eq!(snapshot.connection_id, 7);
        assert_eq!(snapshot.window_duration, Duration::from_secs(5));
        assert_eq!(snapshot.raw_track_notifications, 2);
        assert_eq!(snapshot.raw_account_notifications, 1);
        assert_eq!(snapshot.raw_live_notifications, 5);
        assert_eq!(snapshot.batches, 1);
        assert_eq!(snapshot.max_batch_size, 3);
        assert_eq!(snapshot.track_pushes, 2);
        assert_eq!(snapshot.account_pushes, 1);
        assert_eq!(snapshot.live_pushes, 3);
        assert_eq!(snapshot.live_flushes, 2);
        assert_eq!(snapshot.avg_tracks_per_live_flush, 1.5);
        assert_eq!(snapshot.max_tracks_per_live_flush, 2);
        assert_eq!(snapshot.live_coalesced, 2);
        assert_eq!(snapshot.detail_query_count, 2);
        assert_eq!(snapshot.avg_detail_query, Duration::from_millis(15));
        assert_eq!(snapshot.max_detail_query, Duration::from_millis(18));
        assert_eq!(snapshot.send_count, 2);
        assert_eq!(snapshot.avg_send, Duration::from_millis(7));
        assert_eq!(snapshot.max_send, Duration::from_millis(10));
    }

    #[tokio::test]
    async fn stress_same_track_notifications_emit_deduplicated_diagnostics_snapshot() {
        let repository = seeded_repository();
        repository.set_read_delay(Duration::from_millis(10));
        let (url, state, mut diagnostics_rx) = spawn_server_with_diagnostics(repository, 128).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..48 {
            let _ = state
                .notifications
                .send(ApplicationNotification::TrackChanged {
                    track_id: TrackId::new("btc-core"),
                });
        }

        let _ = recv_event(&mut stream).await;
        let _ = recv_event(&mut stream).await;

        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = state
            .notifications
            .send(ApplicationNotification::AccountChanged);

        let snapshot = tokio::time::timeout(Duration::from_secs(1), diagnostics_rx.recv())
            .await
            .expect("stress test should emit diagnostics snapshot")
            .expect("diagnostics channel should stay open");

        assert_eq!(snapshot.raw_track_notifications, 48);
        assert_eq!(snapshot.raw_account_notifications, 1);
        assert_eq!(snapshot.track_pushes, 1);
        assert_eq!(snapshot.account_pushes, 1);
        assert_eq!(snapshot.detail_query_count, 1);
        assert_eq!(snapshot.max_batch_size, 48);
        assert!(snapshot.avg_detail_query >= Duration::from_millis(25));
    }

    #[tokio::test]
    async fn stress_same_track_live_notifications_emit_coalesced_live_diagnostics_snapshot() {
        let repository = seeded_repository();
        let (url, state, mut diagnostics_rx) = spawn_server_with_diagnostics(repository, 128).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..48 {
            let _ = state
                .websocket_state
                .live_view_notifications
                .send("btc-core".to_string());
        }

        let event = recv_event(&mut stream).await;
        assert!(matches!(
            event,
            StreamEvent::TrackLiveViewChanged { ref track_id, .. } if track_id == "btc-core"
        ));

        let snapshot = tokio::time::timeout(Duration::from_secs(1), diagnostics_rx.recv())
            .await
            .expect("stress test should emit diagnostics snapshot")
            .expect("diagnostics channel should stay open");

        assert_eq!(snapshot.raw_live_notifications, 48);
        assert_eq!(snapshot.live_pushes, 1);
        assert_eq!(snapshot.live_flushes, 1);
        assert_eq!(snapshot.avg_tracks_per_live_flush, 1.0);
        assert_eq!(snapshot.max_tracks_per_live_flush, 1);
        assert_eq!(snapshot.live_coalesced, 47);
        assert_eq!(snapshot.detail_query_count, 0);
        assert_eq!(snapshot.live_query_count, 1);
        assert_eq!(snapshot.send_count, 1);
    }

    fn test_manager() -> TrackManager {
        let mut manager = TrackManager::new(Arc::new(FakeClock));
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
                    out_of_band_policy: BandProtectionPolicy::Freeze {
                        recover: BandRecoverPolicy::BackInBand,
                    },
                },
                CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: 100.0,
                    total_loss_limit: 300.0,
                },
                ExchangeRules {
                    price_tick: 0.0,
                    quantity_step: 0.0,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            )
            .unwrap();
        manager
            .observe(
                &TrackId::new("btc-core"),
                poise_engine::observation::TrackObservation::Market(
                    poise_engine::observation::MarketObservation {
                        mark_price: 95.0,
                        execution_quote: Some(poise_engine::ports::ExecutionQuote {
                            best_bid: 95.0,
                            best_ask: 95.0,
                        }),
                    },
                ),
            )
            .unwrap();
        manager
    }

    fn seed_snapshot_ledger(snapshot: &mut poise_engine::snapshot::TrackRuntimeSnapshot) {
        snapshot.risk.unrealized_pnl = 265.2;
        snapshot.ledger_state.realized_pnl_day =
            Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap());
        snapshot.ledger_state.gross_realized_pnl_today = 980.1;
        snapshot.ledger_state.gross_realized_pnl_cumulative = 980.1;
        snapshot.ledger_state.trading_fee_cumulative = 12.3;
        snapshot.ledger_state.funding_fee_cumulative = -4.0;
        snapshot.ledger_state.unresolved_gaps = vec![
            LedgerGapRecord {
                gap_key: "binance:order_trade_update:btcusdt:12345:commission_asset".into(),
                reason: LedgerGapReason::UnsupportedCommissionAsset,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                source: "ORDER_TRADE_UPDATE".into(),
            },
            LedgerGapRecord {
                gap_key: "binance:funding_fee:btcusdt:2026-03-24T08:00:00+00:00:missing_symbol"
                    .into(),
                reason: LedgerGapReason::MissingSymbol,
                observed_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
                source: "ACCOUNT_UPDATE:FUNDING_FEE".into(),
            },
        ];
    }

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    #[derive(Default)]
    struct TestRepository {
        snapshots: Mutex<HashMap<String, StoredTrackSnapshot>>,
        events: Mutex<HashMap<String, Vec<StoredTrackEvent>>>,
        effects: Mutex<Vec<PersistedTrackEffect>>,
        next_event_id: Mutex<i64>,
        read_delay: Mutex<Option<Duration>>,
        load_snapshot_error: Mutex<Option<String>>,
        load_snapshot_calls: Mutex<usize>,
    }

    impl TestRepository {
        fn seed_snapshot(&self, snapshot: poise_engine::snapshot::TrackRuntimeSnapshot) {
            self.snapshots.lock().unwrap().insert(
                snapshot.track_id.as_str().to_string(),
                StoredTrackSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                },
            );
        }

        fn set_read_delay(&self, delay: Duration) {
            *self.read_delay.lock().unwrap() = Some(delay);
        }

        fn remove_snapshot(&self, track_id: &str) {
            self.snapshots.lock().unwrap().remove(track_id);
        }

        fn set_load_snapshot_error(&self, error: &str) {
            *self.load_snapshot_error.lock().unwrap() = Some(error.to_string());
        }

        fn load_snapshot_calls(&self) -> usize {
            *self.load_snapshot_calls.lock().unwrap()
        }

        fn seed_pending_noop_effect(&self) {
            self.effects.lock().unwrap().push(PersistedTrackEffect {
                effect_id: "effect-1".into(),
                track_id: TrackId::new("btc-core"),
                batch_id: "batch-1".into(),
                sequence: 0,
                effect: TrackEffect::NoOp,
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
        }

        async fn maybe_delay_read(&self) {
            let delay = *self.read_delay.lock().unwrap();
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
        }
    }

    #[async_trait::async_trait]
    impl TrackMutationStore for TestRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &poise_engine::snapshot::TrackRuntimeSnapshot,
            events: &[poise_core::events::DomainEvent],
            effects: &[TrackEffect],
            effect_status_update: Option<&EffectStatusUpdate>,
        ) -> Result<CommittedTrackWrite> {
            let now = Utc::now();
            self.snapshots.lock().unwrap().insert(
                id.to_string(),
                StoredTrackSnapshot {
                    snapshot: state.clone(),
                    updated_at: now,
                },
            );

            if !events.is_empty() {
                let mut next_event_id = self.next_event_id.lock().unwrap();
                let mut stored_events = self.events.lock().unwrap();
                let entry = stored_events.entry(id.to_string()).or_default();
                for event in events {
                    *next_event_id += 1;
                    entry.push(StoredTrackEvent {
                        id: *next_event_id,
                        track_id: TrackId::new(id),
                        event: event.clone(),
                        created_at: now,
                    });
                }
            }

            let persisted_effects: Vec<_> = effects
                .iter()
                .enumerate()
                .map(|(index, effect)| PersistedTrackEffect {
                    effect_id: format!("{id}:effect:{index}"),
                    track_id: TrackId::new(id),
                    batch_id: format!("{id}:batch"),
                    sequence: index as u32,
                    effect: effect.clone(),
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: now,
                    updated_at: now,
                })
                .collect();
            self.effects
                .lock()
                .unwrap()
                .extend(persisted_effects.iter().cloned());
            if let Some(effect_status_update) = effect_status_update
                && let Some(effect) = self
                    .effects
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|effect| effect.effect_id == effect_status_update.effect_id)
            {
                effect.status = effect_status_update.status;
                effect.attempt_count += effect_status_update.attempt_delta;
                effect.last_error = effect_status_update.last_error.clone();
                effect.updated_at = now;
            }

            Ok(CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(
            &self,
            id: &str,
        ) -> Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(|stored| stored.snapshot))
        }

        async fn list_track_events(
            &self,
            id: &str,
        ) -> Result<Vec<poise_core::events::DomainEvent>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|stored| stored.event)
                .collect())
        }
    }

    #[async_trait::async_trait]
    impl TrackEffectStore for TestRepository {
        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
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
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
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
                .filter(|effect| effect.track_id == *track_id)
                .filter(|effect| effect.batch_id == batch_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn save_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackQueryStore for TestRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            self.maybe_delay_read().await;
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            self.maybe_delay_read().await;
            *self.load_snapshot_calls.lock().unwrap() += 1;
            if let Some(error) = self.load_snapshot_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned())
        }

        async fn list_recent_track_events(
            &self,
            track_id: &TrackId,
            limit: usize,
        ) -> Result<Vec<StoredTrackEvent>> {
            self.maybe_delay_read().await;
            let mut events = self
                .events
                .lock()
                .unwrap()
                .get(track_id.as_str())
                .cloned()
                .unwrap_or_default();
            if events.len() > limit {
                events = events.split_off(events.len() - limit);
            }
            Ok(events)
        }

        async fn list_recent_track_effects(
            &self,
            track_id: &TrackId,
            limit: usize,
        ) -> Result<Vec<PersistedTrackEffect>> {
            self.maybe_delay_read().await;
            let mut effects: Vec<_> = self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.track_id == *track_id)
                .cloned()
                .collect();
            effects.sort_by_key(|effect| effect.updated_at);
            if effects.len() > limit {
                effects = effects.split_off(effects.len() - limit);
            }
            Ok(effects)
        }
    }

    struct NoopExchange;

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for NoopExchange {
        async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
            Ok(poise_engine::ports::AccountSummarySnapshot {
                equity: 1_000_000.0,
                available: 1_000_000.0,
                unrealized_pnl: 0.0,
                observed_at: Utc::now(),
            })
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::ExecutionPort for NoopExchange {
        async fn submit_order(
            &self,
            req: poise_engine::ports::OrderRequest,
        ) -> Result<poise_engine::ports::OrderReceipt> {
            Ok(poise_engine::ports::OrderReceipt {
                order_id: "noop-order".into(),
                client_order_id: req.client_order_id,
                status: poise_engine::ports::OrderStatus::New,
            })
        }

        async fn cancel_order(
            &self,
            _instrument: &poise_engine::track::Instrument,
            _order_id: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn cancel_all(&self, _instrument: &poise_engine::track::Instrument) -> Result<()> {
            Ok(())
        }

        async fn get_position(
            &self,
            instrument: &poise_engine::track::Instrument,
        ) -> Result<poise_engine::ports::Position> {
            Ok(poise_engine::ports::Position {
                instrument: instrument.clone(),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &poise_engine::track::Instrument,
        ) -> Result<Vec<poise_engine::ports::ExchangeOrder>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountPort for NoopExchange {
        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &poise_engine::track::Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn subscribe_user_data(
            &self,
        ) -> Result<tokio::sync::mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
            let (_sender, receiver) = tokio::sync::mpsc::channel(1);
            Ok(receiver)
        }
    }
}
