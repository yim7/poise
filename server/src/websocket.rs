use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use poise_application::ApplicationNotification;
use poise_protocol::StreamEvent;

use crate::server_context::WebSocketState;

pub async fn ws_handler(ws: WebSocketUpgrade, state: WebSocketState) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
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

    loop {
        match receiver.recv().await {
            Ok(ApplicationNotification::TrackChanged { track_id }) => {
                if !push_projected_updates(&mut socket, &state, track_id).await {
                    break;
                }
            }
            Ok(ApplicationNotification::AccountChanged) => {
                if !push_account_summary(&mut socket, &state).await {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    "websocket notification stream lagged by {skipped} messages; closing socket for resync"
                );
                close_socket(&mut socket).await;
                break;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn push_projected_updates(
    socket: &mut WebSocket,
    state: &WebSocketState,
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
        if !send_event(socket, event).await {
            return false;
        }
    }

    true
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

async fn push_account_summary(socket: &mut WebSocket, state: &WebSocketState) -> bool {
    let Some(summary) = state.account_monitor.current_summary().await else {
        return true;
    };
    send_event(
        socket,
        StreamEvent::AccountSummaryChanged {
            summary: state.account_projector.project_summary(&summary),
        },
    )
    .await
}

async fn send_event(socket: &mut WebSocket, event: StreamEvent) -> bool {
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
    use chrono::{TimeZone, Utc};
    use futures_util::{SinkExt, StreamExt};
    use poise_application::{
        CommittedTrackWrite, EffectStatus, EffectStatusUpdate, FollowUpRetirementRequest,
        PersistedTrackEffect, StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore,
        TrackMutationStore, TrackQueryStore,
    };
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::ExchangeRules;
    use poise_engine::command::TrackCommand;
    use poise_engine::ledger::{LedgerGapReason, LedgerGapRecord};
    use poise_engine::manager::TrackManager;
    use poise_engine::ports::{
        AccountSummarySnapshot, ClockPort, ExchangeInfo, ExchangeOrder, ExchangePort, OrderReceipt,
        OrderRequest, Position,
    };
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;
    use poise_protocol::{
        ExecutionStateView, ExecutionStatusView, RiskSignalView, StreamEvent, TrackStatus,
    };
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::server_context::WebSocketState;
    use crate::effect_worker::EffectWorker;
    use crate::projector::TrackProjector;
    use crate::test_support::{
        build_effect_worker_test_context, build_test_application_services, build_websocket_state,
        test_budget_catalog, unavailable_account_monitor,
    };
    use poise_application::{
        AccountMonitor, AccountMonitorConfig, AccountMonitorStore, ApplicationNotification,
        StoredAccountMonitorState, TrackCommandService, TrackQueryService,
    };

    use super::ws_handler_with_test_state;

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

    async fn spawn_server(
        repository: Arc<TestRepository>,
    ) -> (String, WebSocketTestContext) {
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
        let query_service = Arc::new(TrackQueryService::new(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_budget_catalog("btc-core"),
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
        let query_service = Arc::new(TrackQueryService::new(
            repository.clone() as Arc<dyn TrackQueryStore>,
            test_budget_catalog("btc-core"),
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

        build_effect_worker_test_context(&services, mutation_store, effect_store).effect_worker_state
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

        let _ = state.notifications.send(ApplicationNotification::TrackChanged {
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

        let _ = state.notifications.send(
            poise_application::ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            },
        );

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
        let (url, state) =
            spawn_server_with_account_monitor(repository, account_monitor).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        let _ = state.notifications.send(ApplicationNotification::AccountChanged);

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
    async fn closes_socket_when_notification_stream_lags() {
        let repository = seeded_repository();
        repository.set_read_delay(Duration::from_millis(50));
        let (url, state) = spawn_server_with_capacity(repository, 1).await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        for _ in 0..8 {
            let _ = state.notifications.send(ApplicationNotification::TrackChanged {
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

        let _ = state.notifications.send(ApplicationNotification::TrackChanged {
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

        let _ = state.notifications.send(ApplicationNotification::TrackChanged {
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
                    out_of_band_policy: OutOfBandPolicy::Freeze,
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
                        reference_price: 95.0,
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

        async fn get_account_capacity_snapshot(
            &self,
            _instrument: &Instrument,
        ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
            Ok(poise_engine::ports::AccountCapacitySnapshot {
                max_increase_notional: 1_000_000.0,
            })
        }

        async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
            Ok(Utc::now())
        }
    }
}
