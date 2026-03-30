use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use grid_protocol::{GridStreamEvent, GridStreamPayload};

use crate::assembly::ServerState;
use crate::notifications::GridInternalNotification;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    let mut receiver = state.write_service.subscribe_notifications();

    loop {
        let grid_id = match receiver.recv().await {
            Ok(GridInternalNotification::GridWriteCommitted { grid_id, .. })
            | Ok(GridInternalNotification::GridEffectStateChanged { grid_id }) => grid_id,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    "websocket notification stream lagged by {skipped} messages; closing socket for resync"
                );
                close_socket(&mut socket).await;
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        if !push_projected_updates(&mut socket, &state, grid_id).await {
            break;
        }
    }
}

async fn push_projected_updates(
    socket: &mut WebSocket,
    state: &ServerState,
    grid_id: grid_engine::grid::GridId,
) -> bool {
    let source = match state.query_service.load_detail_source(&grid_id).await {
        Ok(Some(source)) => source,
        Ok(None) => {
            tracing::warn!(
                "grid `{}` missing from read model during websocket push; closing socket for resync",
                grid_id.as_str()
            );
            close_socket(socket).await;
            return false;
        }
        Err(error) => {
            tracing::warn!(
                "failed to load read model for websocket grid `{}`: {error}; closing socket for resync",
                grid_id.as_str()
            );
            close_socket(socket).await;
            return false;
        }
    };

    let grid_id_text = grid_id.as_str().to_string();
    let list_item = state.projector.project_list_item(&source);
    let detail = state.projector.project_detail(&source);
    let events = [
        GridStreamEvent {
            grid_id: grid_id_text.clone(),
            payload: GridStreamPayload::GridListItemChanged { item: list_item },
        },
        GridStreamEvent {
            grid_id: grid_id_text,
            payload: GridStreamPayload::GridDetailChanged { detail },
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

async fn send_event(socket: &mut WebSocket, event: GridStreamEvent) -> bool {
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
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::ExchangeRules;
    use grid_engine::command::GridCommand;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::manager::GridManager;
    use grid_engine::ports::{
        ClockPort, EffectStatus, ExchangeInfo, ExchangeOrder, ExchangePort, GridReadRepositoryPort,
        OrderReceipt, OrderRequest, PersistedGridEffect, Position, StateRepositoryPort,
        StoredDomainEvent, StoredGridSnapshot,
    };
    use grid_engine::transition::GridEffect;
    use grid_protocol::{
        ExecutionStateView, ExecutionStatusView, GridStatus, GridStreamEvent, GridStreamPayload,
    };
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::assembly::{ServerState, build_server_state};
    use crate::effect_service::EffectService;
    use crate::effect_worker::EffectWorker;
    use crate::notifications::GridInternalNotification;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::ws_handler;

    type ClientStream = futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >;

    async fn spawn_server(
        repository: Arc<TestRepository>,
    ) -> (String, Arc<GridWriteService>, ServerState) {
        spawn_server_with_capacity(repository, 16).await
    }

    async fn spawn_server_with_capacity(
        repository: Arc<TestRepository>,
        notification_capacity: usize,
    ) -> (String, Arc<GridWriteService>, ServerState) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(notification_capacity);
        let effect_service = Arc::new(EffectService::new(
            repository.clone() as Arc<dyn StateRepositoryPort>
        ));
        let service = Arc::new(GridWriteService::new(
            test_manager(),
            repository.clone() as Arc<dyn StateRepositoryPort>,
            notifications,
        ));
        let state = build_server_state(
            Arc::clone(&service),
            effect_service,
            Arc::new(GridQueryService::new(
                repository.clone() as Arc<dyn GridReadRepositoryPort>
            )),
            Arc::new(GridProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("ws://{address}/ws"), service, state)
    }

    async fn recv_event(stream: &mut ClientStream) -> GridStreamEvent {
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

        service.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
            grid_id: GridId::new("btc-core"),
            recovery_anomaly_active: false,
        });

        let payload_a = recv_event(&mut stream_a).await;
        let payload_b = recv_event(&mut stream_b).await;

        assert_eq!(payload_a, payload_b);
        assert_eq!(payload_a.grid_id, "btc-core");
        assert!(matches!(
            payload_a.payload,
            GridStreamPayload::GridListItemChanged { .. }
        ));
    }

    #[tokio::test]
    async fn broadcasts_grid_detail_changed_after_write_commit() {
        let repository = seeded_repository();
        let (url, service, _) = spawn_server(repository).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        service
            .command("btc-core", GridCommand::Pause)
            .await
            .unwrap();

        let first = recv_event(&mut stream).await;
        let second = recv_event(&mut stream).await;
        let events = [first, second];

        assert!(
            events.iter().any(|event| matches!(
                event.payload,
                GridStreamPayload::GridListItemChanged { .. }
            ))
        );
        let detail = events
            .iter()
            .find_map(|event| match &event.payload {
                GridStreamPayload::GridDetailChanged { detail } => Some(detail),
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
                GridStreamPayload::GridListItemChanged { item } => Some(item),
                _ => None,
            })
            .expect("should emit projected list item change");
        assert_eq!(item.id, "btc-core");
        assert_eq!(item.execution.execution_status, ExecutionStatusView::Normal);
        assert_eq!(item.execution.active_slot_count, 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.payload, GridStreamPayload::GridDetailChanged { .. }))
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
            service.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new("btc-core"),
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

        service.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
            grid_id: GridId::new("btc-core"),
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

        service.emit_internal_notification(GridInternalNotification::GridWriteCommitted {
            grid_id: GridId::new("btc-core"),
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

    fn test_manager() -> GridManager {
        let mut manager = GridManager::new(Arc::new(FakeClock));
        manager
            .add_grid(
                GridId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
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
                    price_tick: 0.0,
                    quantity_step: 0.0,
                    min_qty: 0.0,
                    min_notional: 0.0,
                },
            )
            .unwrap();
        manager
            .observe(
                &GridId::new("btc-core"),
                grid_engine::observation::GridObservation::Market(
                    grid_engine::observation::MarketObservation {
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
        snapshots: Mutex<HashMap<String, StoredGridSnapshot>>,
        events: Mutex<HashMap<String, Vec<StoredDomainEvent>>>,
        effects: Mutex<Vec<PersistedGridEffect>>,
        next_event_id: Mutex<i64>,
        read_delay: Mutex<Option<Duration>>,
        load_snapshot_error: Mutex<Option<String>>,
    }

    impl TestRepository {
        fn seed_snapshot(&self, snapshot: grid_engine::ports::GridSnapshot) {
            self.snapshots.lock().unwrap().insert(
                snapshot.grid_id.as_str().to_string(),
                StoredGridSnapshot {
                    snapshot,
                    updated_at: Utc::now(),
                },
            );
        }

        fn set_read_delay(&self, delay: Duration) {
            *self.read_delay.lock().unwrap() = Some(delay);
        }

        fn remove_snapshot(&self, grid_id: &str) {
            self.snapshots.lock().unwrap().remove(grid_id);
        }

        fn set_load_snapshot_error(&self, error: &str) {
            *self.load_snapshot_error.lock().unwrap() = Some(error.to_string());
        }

        fn seed_pending_noop_effect(&self) {
            self.effects.lock().unwrap().push(PersistedGridEffect {
                effect_id: "effect-1".into(),
                grid_id: GridId::new("btc-core"),
                batch_id: "batch-1".into(),
                sequence: 0,
                effect: GridEffect::NoOp,
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
            state: &grid_engine::ports::GridSnapshot,
            events: &[grid_core::events::DomainEvent],
            effects: &[GridEffect],
            effect_status_update: Option<&grid_engine::ports::EffectStatusUpdate>,
        ) -> Result<grid_engine::ports::CommittedGridWrite> {
            let now = Utc::now();
            self.snapshots.lock().unwrap().insert(
                id.to_string(),
                StoredGridSnapshot {
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
                    entry.push(StoredDomainEvent {
                        id: *next_event_id,
                        grid_id: GridId::new(id),
                        event: event.clone(),
                        created_at: now,
                    });
                }
            }

            let persisted_effects: Vec<_> = effects
                .iter()
                .enumerate()
                .map(|(index, effect)| PersistedGridEffect {
                    effect_id: format!("{id}:effect:{index}"),
                    grid_id: GridId::new(id),
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

            Ok(grid_engine::ports::CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: persisted_effects,
            })
        }

        async fn load_grid_state(
            &self,
            id: &str,
        ) -> Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .map(|stored| stored.snapshot))
        }

        async fn list_events(&self, id: &str) -> Result<Vec<grid_core::events::DomainEvent>> {
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

        async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn list_pending_submit_effects_for_grid(
            &self,
            grid_id: &GridId,
        ) -> Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.grid_id == *grid_id)
                .filter(|effect| effect.status == EffectStatus::Pending)
                .filter(|effect| matches!(effect.effect, GridEffect::SubmitOrder { .. }))
                .cloned()
                .collect())
        }

        async fn mark_effect_executing(&self, effect_id: &str) -> Result<()> {
            if let Some(effect) = self
                .effects
                .lock()
                .unwrap()
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
            {
                effect.status = EffectStatus::Executing;
                effect.updated_at = Utc::now();
            }
            Ok(())
        }

        async fn mark_effect_succeeded(&self, effect_id: &str) -> Result<()> {
            if let Some(effect) = self
                .effects
                .lock()
                .unwrap()
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
            {
                effect.status = EffectStatus::Succeeded;
                effect.updated_at = Utc::now();
            }
            Ok(())
        }

        async fn mark_effect_superseded(&self, effect_id: &str) -> Result<()> {
            if let Some(effect) = self
                .effects
                .lock()
                .unwrap()
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
            {
                effect.status = EffectStatus::Superseded;
                effect.updated_at = Utc::now();
            }
            Ok(())
        }

        async fn mark_effect_failed(&self, effect_id: &str, error: &str) -> Result<()> {
            if let Some(effect) = self
                .effects
                .lock()
                .unwrap()
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
            {
                effect.status = EffectStatus::Failed;
                effect.last_error = Some(error.to_string());
                effect.updated_at = Utc::now();
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl GridReadRepositoryPort for TestRepository {
        async fn list_grid_snapshots(&self) -> Result<Vec<StoredGridSnapshot>> {
            self.maybe_delay_read().await;
            Ok(self.snapshots.lock().unwrap().values().cloned().collect())
        }

        async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<StoredGridSnapshot>> {
            self.maybe_delay_read().await;
            if let Some(error) = self.load_snapshot_error.lock().unwrap().clone() {
                return Err(anyhow!(error));
            }
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(grid_id.as_str())
                .cloned())
        }

        async fn list_recent_grid_events(
            &self,
            grid_id: &GridId,
            limit: usize,
        ) -> Result<Vec<StoredDomainEvent>> {
            self.maybe_delay_read().await;
            let mut events = self
                .events
                .lock()
                .unwrap()
                .get(grid_id.as_str())
                .cloned()
                .unwrap_or_default();
            if events.len() > limit {
                events = events.split_off(events.len() - limit);
            }
            Ok(events)
        }

        async fn list_recent_grid_effects(
            &self,
            grid_id: &GridId,
            limit: usize,
        ) -> Result<Vec<PersistedGridEffect>> {
            self.maybe_delay_read().await;
            let mut effects: Vec<_> = self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.grid_id == *grid_id)
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
                },
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }
}
