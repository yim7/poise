use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use poise_protocol::{TrackStreamEvent, TrackStreamPayload};

use crate::assembly::ServerState;
use crate::notifications::TrackInternalNotification;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    let mut receiver = state.write_service.subscribe_notifications();

    loop {
        let track_id = match receiver.recv().await {
            Ok(TrackInternalNotification::TrackWriteCommitted { track_id, .. })
            | Ok(TrackInternalNotification::TrackEffectStateChanged { track_id }) => track_id,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    "websocket notification stream lagged by {skipped} messages; closing socket for resync"
                );
                close_socket(&mut socket).await;
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        if !push_projected_updates(&mut socket, &state, track_id).await {
            break;
        }
    }
}

async fn push_projected_updates(
    socket: &mut WebSocket,
    state: &ServerState,
    track_id: poise_engine::track::TrackId,
) -> bool {
    let source = match state
        .query_service
        .load_track_detail_source(&track_id)
        .await
    {
        Ok(Some(source)) => source,
        Ok(None) => {
            tracing::warn!(
                "track `{}` missing from read model during websocket push; closing socket for resync",
                track_id.as_str()
            );
            close_socket(socket).await;
            return false;
        }
        Err(error) => {
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
        TrackStreamEvent {
            track_id: track_id_text.clone(),
            payload: TrackStreamPayload::TrackListItemChanged { item: list_item },
        },
        TrackStreamEvent {
            track_id: track_id_text,
            payload: TrackStreamPayload::TrackDetailChanged { detail },
        },
    ];

    for event in events {
        if !send_event(socket, event).await {
            return false;
        }
    }

    true
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

async fn send_event(socket: &mut WebSocket, event: TrackStreamEvent) -> bool {
    let message = match serde_json::to_string(&event) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!("failed to serialize websocket event: {error}");
            return true;
        }
    };

    socket.send(Message::Text(message)).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use axum::Router;
    use chrono::Utc;
    use futures_util::StreamExt;
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use poise_engine::command::TrackCommand;
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        ClockPort, EffectStatus, ExchangeInfo, ExchangeOrder, ExchangePort, OrderReceipt,
        OrderRequest, PersistedTrackEffect, Position, StateRepositoryPort, StoredTrackEvent,
        StoredTrackSnapshot, TrackReadRepositoryPort,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_protocol::{
        ExecutionStateView, ExecutionStatusView, GridStatus, TrackStreamEvent, TrackStreamPayload,
    };
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::assembly::{ServerState, build_server_state};
    use crate::effect_worker::EffectWorker;
    use crate::notifications::TrackInternalNotification;
    use crate::projector::TrackProjector;
    use crate::query_service::TrackQueryService;
    use crate::write_service::TrackWriteService;

    use super::ws_handler;

    type ClientStream = futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >;

    async fn spawn_server(
        repository: Arc<TestRepository>,
    ) -> (String, Arc<TrackWriteService>, ServerState) {
        spawn_server_with_capacity(repository, 16).await
    }

    async fn spawn_server_with_capacity(
        repository: Arc<TestRepository>,
        notification_capacity: usize,
    ) -> (String, Arc<TrackWriteService>, ServerState) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(notification_capacity);
        let state_repository = repository.clone() as Arc<dyn StateRepositoryPort>;
        let service = Arc::new(TrackWriteService::new(
            test_manager(),
            state_repository.clone(),
            notifications,
        ));
        let state = build_server_state(
            Arc::clone(&service),
            state_repository,
            Arc::new(TrackQueryService::new(
                repository.clone() as Arc<dyn TrackReadRepositoryPort>
            )),
            Arc::new(TrackProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("ws://{address}/ws"), service, state)
    }

    async fn recv_event(stream: &mut ClientStream) -> TrackStreamEvent {
        let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    fn seeded_repository() -> Arc<TestRepository> {
        let repository = Arc::new(TestRepository::default());
        repository.seed_snapshot(test_manager().snapshot("btc-core").unwrap());
        repository
    }

    #[tokio::test]
    async fn broadcasts_events_to_multiple_clients() {
        let repository = seeded_repository();
        let (url, service, _) = spawn_server(repository).await;
        let (client_a, _) = connect_async(&url).await.unwrap();
        let (client_b, _) = connect_async(&url).await.unwrap();
        let (_, mut stream_a) = client_a.split();
        let (_, mut stream_b) = client_b.split();

        service.emit_internal_notification(TrackInternalNotification::TrackWriteCommitted {
            track_id: TrackId::new("btc-core"),
            recovery_anomaly_active: false,
        });

        let payload_a = recv_event(&mut stream_a).await;
        let payload_b = recv_event(&mut stream_b).await;

        assert_eq!(payload_a, payload_b);
        assert_eq!(payload_a.track_id, "btc-core");
        assert!(matches!(
            payload_a.payload,
            TrackStreamPayload::TrackListItemChanged { .. }
        ));
    }

    #[tokio::test]
    async fn broadcasts_grid_detail_changed_after_write_commit() {
        let repository = seeded_repository();
        let (url, service, _) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        service
            .command("btc-core", TrackCommand::Pause)
            .await
            .unwrap();

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        assert!(events.iter().any(|event| matches!(
            event.payload,
            TrackStreamPayload::TrackListItemChanged { .. }
        )));
        let detail = events
            .iter()
            .find_map(|event| match &event.payload {
                TrackStreamPayload::TrackDetailChanged { detail } => Some(detail),
                _ => None,
            })
            .expect("should emit projected detail change");
        assert_eq!(detail.identity.id, "btc-core");
        assert_eq!(detail.status.lifecycle.status, GridStatus::Paused);
        assert_eq!(detail.execution.state, ExecutionStateView::Paused);
    }

    #[tokio::test]
    async fn broadcasts_grid_list_item_changed_after_effect_state_change() {
        let repository = seeded_repository();
        repository.seed_pending_noop_effect();
        let (url, service, state) = spawn_server(repository).await;
        let worker = EffectWorker::new(state, Arc::new(NoopExchange), Duration::from_millis(10));
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        worker.run_once().await.unwrap();

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        let item = events
            .iter()
            .find_map(|event| match &event.payload {
                TrackStreamPayload::TrackListItemChanged { item } => Some(item),
                _ => None,
            })
            .expect("should emit projected list item change");
        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_slot_count, 1);
        assert!(
            events.iter().any(|event| matches!(
                event.payload,
                TrackStreamPayload::TrackDetailChanged { .. }
            ))
        );

        drop(service);
    }

    #[tokio::test]
    async fn closes_socket_when_notification_stream_lags() {
        let repository = seeded_repository();
        repository.set_read_delay(Duration::from_millis(50));
        let (url, service, _) = spawn_server_with_capacity(repository, 1).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..8 {
            service.emit_internal_notification(TrackInternalNotification::TrackWriteCommitted {
                track_id: TrackId::new("btc-core"),
                recovery_anomaly_active: false,
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
    async fn closes_socket_when_grid_read_model_is_missing_for_notification() {
        let repository = seeded_repository();
        repository.remove_snapshot("btc-core");
        let (url, service, _) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        service.emit_internal_notification(TrackInternalNotification::TrackWriteCommitted {
            track_id: TrackId::new("btc-core"),
            recovery_anomaly_active: false,
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
    async fn closes_socket_when_grid_read_model_load_fails() {
        let repository = seeded_repository();
        repository.set_load_snapshot_error("injected read failure");
        let (url, service, _) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        service.emit_internal_notification(TrackInternalNotification::TrackWriteCommitted {
            track_id: TrackId::new("btc-core"),
            recovery_anomaly_active: false,
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
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: OutOfBandPolicy::Freeze,
                },
                CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -100.0,
                    stop_loss_pct: 10.0,
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
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();
        manager
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
    }

    impl TestRepository {
        fn seed_snapshot(&self, snapshot: poise_engine::ports::TrackSnapshot) {
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
    impl StateRepositoryPort for TestRepository {
        async fn save_transition_with_effect_status(
            &self,
            id: &str,
            state: &poise_engine::ports::TrackSnapshot,
            events: &[poise_core::events::DomainEvent],
            effects: &[TrackEffect],
            effect_status_update: Option<&poise_engine::ports::EffectStatusUpdate>,
        ) -> Result<poise_engine::ports::CommittedTrackWrite> {
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
            if let Some(effect_status_update) = effect_status_update {
                if let Some(effect) = self
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
            }

            Ok(poise_engine::ports::CommittedTrackWrite {
                track_id: TrackId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_track_state(
            &self,
            id: &str,
        ) -> Result<Option<poise_engine::ports::TrackSnapshot>> {
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
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }

        async fn list_follow_up_retirement_requests(
            &self,
            _track_id: &TrackId,
        ) -> Result<Vec<poise_engine::ports::FollowUpRetirementRequest>> {
            Ok(Vec::new())
        }

        async fn delete_follow_up_retirement_request(
            &self,
            _track_id: &TrackId,
            _request: &poise_engine::ports::FollowUpRetirementRequest,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl TrackReadRepositoryPort for TestRepository {
        async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
            self.maybe_delay_read().await;
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_track_snapshot(
            &self,
            track_id: &TrackId,
        ) -> Result<Option<StoredTrackSnapshot>> {
            self.maybe_delay_read().await;
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
    impl ExchangePort for NoopExchange {
        async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
            Err(anyhow!("submit_order should not be called"))
        }

        async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
            Err(anyhow!("cancel_order should not be called"))
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
            Err(anyhow!("cancel_all should not be called"))
        }

        async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
            Ok(Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
            Ok(ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: ExchangeRules {
                    price_tick: 0.0,
                    quantity_step: 0.0,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            })
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

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }
}
