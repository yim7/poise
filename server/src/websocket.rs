use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
#[cfg_attr(not(test), allow(dead_code))]
pub type WsEvent = grid_protocol::WsEvent;

use crate::assembly::ServerState;
use crate::notifications::GridInternalNotification;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    let mut receiver = state.write_service.subscribe_notifications();

    loop {
        let notification = match receiver.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        let event = match notification {
            GridInternalNotification::GridWriteCommitted { grid_id }
            | GridInternalNotification::GridEffectStateChanged { grid_id } => WsEvent {
                grid_id: grid_id.as_str().to_string(),
                event: grid_protocol::DomainEvent::SnapshotUpdated,
            },
        };

        let message = match serde_json::to_string(&event) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!("failed to serialize websocket event: {error}");
                continue;
            }
        };

        if socket.send(Message::Text(message)).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::Router;
    use chrono::Utc;
    use futures_util::StreamExt;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::ExchangeRules;
    use grid_engine::command::GridCommand;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_engine::ports::{
        EffectStatus, ExchangeInfo, ExchangeOrder, OrderReceipt, OrderRequest, PersistedGridEffect,
        Position, StateRepositoryPort,
    };
    use grid_engine::transition::GridEffect;
    use grid_protocol::DomainEvent;
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::assembly::{NoopReadRepository, build_server_state};
    use crate::effect_worker::EffectWorker;
    use crate::notifications::GridInternalNotification;
    use crate::projector::GridProjector;
    use crate::query_service::GridQueryService;
    use crate::write_service::GridWriteService;

    use super::{WsEvent, ws_handler};

    async fn spawn_server() -> (
        String,
        Arc<GridWriteService>,
        tokio::sync::broadcast::Sender<GridInternalNotification>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (notifications, _) = tokio::sync::broadcast::channel(16);
        let mut manager = grid_engine::manager::GridManager::new(std::sync::Arc::new(FakeClock));
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
        let service = Arc::new(GridWriteService::new(
            manager,
            std::sync::Arc::new(FakePersistence),
            notifications.clone(),
        ));
        let state = build_server_state(
            Arc::clone(&service),
            Arc::new(GridQueryService::new(Arc::new(FakeReadRepository))),
            Arc::new(GridProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("ws://{address}/ws"), service, notifications)
    }

    #[tokio::test]
    async fn broadcasts_events_to_multiple_clients() {
        let (url, _service, events) = spawn_server().await;
        let (client_a, _) = connect_async(&url).await.unwrap();
        let (client_b, _) = connect_async(&url).await.unwrap();
        let (_, mut stream_a) = client_a.split();
        let (_, mut stream_b) = client_b.split();

        events
            .send(GridInternalNotification::GridWriteCommitted {
                grid_id: GridId::new("btc-core"),
            })
            .unwrap();

        let message_a = tokio::time::timeout(Duration::from_secs(1), stream_a.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let message_b = tokio::time::timeout(Duration::from_secs(1), stream_b.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let payload_a: WsEvent = serde_json::from_str(message_a.to_text().unwrap()).unwrap();
        let payload_b: WsEvent = serde_json::from_str(message_b.to_text().unwrap()).unwrap();

        assert_eq!(payload_a, payload_b);
        assert_eq!(payload_a.grid_id, "btc-core");
    }

    #[tokio::test]
    async fn broadcasts_events_from_persisted_transition() {
        let (url, service, _events) = spawn_server().await;
        let (client, _) = connect_async(&url).await.unwrap();
        let (_, mut stream) = client.split();

        service
            .command("btc-core", GridCommand::Pause)
            .await
            .unwrap();

        let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let payload: WsEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();

        assert_eq!(payload.grid_id, "btc-core");
        assert_eq!(payload.event, DomainEvent::SnapshotUpdated);
    }

    #[tokio::test]
    async fn broadcasts_effect_state_changed_from_effect_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (write_notifications, _) = tokio::sync::broadcast::channel(16);
        let service = Arc::new(GridWriteService::new(
            test_manager(),
            Arc::new(PendingEffectPersistence::new()) as Arc<dyn StateRepositoryPort>,
            write_notifications,
        ));
        let state = build_server_state(
            Arc::clone(&service),
            Arc::new(GridQueryService::new(Arc::new(NoopReadRepository))),
            Arc::new(GridProjector::new()),
        );
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let worker = EffectWorker::new(state, Arc::new(NoopExchange), Duration::from_millis(10));
        let (client, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        let (_, mut stream) = client.split();

        worker.run_once().await.unwrap();

        let message = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let payload: WsEvent = serde_json::from_str(message.to_text().unwrap()).unwrap();

        assert_eq!(payload.grid_id, "btc-core");
        assert_eq!(payload.event, DomainEvent::SnapshotUpdated);
    }

    fn test_manager() -> grid_engine::manager::GridManager {
        let mut manager = grid_engine::manager::GridManager::new(std::sync::Arc::new(FakeClock));
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
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl grid_engine::ports::StateRepositoryPort for FakePersistence {
        async fn save_transition(
            &self,
            id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            _effects: &[grid_engine::transition::GridEffect],
        ) -> anyhow::Result<grid_engine::ports::CommittedGridWrite> {
            Ok(grid_engine::ports::CommittedGridWrite {
                grid_id: grid_engine::grid::GridId::new(id),
                effects: Vec::new(),
            })
        }

        async fn load_grid_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(
            &self,
        ) -> anyhow::Result<Vec<grid_engine::ports::PersistedGridEffect>> {
            Ok(Vec::new())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_succeeded(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_failed(&self, _effect_id: &str, _error: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct FakeReadRepository;

    #[async_trait::async_trait]
    impl grid_engine::ports::GridReadRepositoryPort for FakeReadRepository {
        async fn list_grid_snapshots(
            &self,
        ) -> anyhow::Result<Vec<grid_engine::ports::StoredGridSnapshot>> {
            Ok(Vec::new())
        }

        async fn load_grid_snapshot(
            &self,
            _grid_id: &grid_engine::grid::GridId,
        ) -> anyhow::Result<Option<grid_engine::ports::StoredGridSnapshot>> {
            Ok(None)
        }

        async fn list_recent_grid_events(
            &self,
            _grid_id: &grid_engine::grid::GridId,
            _limit: usize,
        ) -> anyhow::Result<Vec<grid_engine::ports::StoredDomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_recent_grid_effects(
            &self,
            _grid_id: &grid_engine::grid::GridId,
            _limit: usize,
        ) -> anyhow::Result<Vec<grid_engine::ports::PersistedGridEffect>> {
            Ok(Vec::new())
        }
    }

    struct PendingEffectPersistence {
        effects: Mutex<Vec<PersistedGridEffect>>,
    }

    impl PendingEffectPersistence {
        fn new() -> Self {
            Self {
                effects: Mutex::new(vec![PersistedGridEffect {
                    effect_id: "effect-1".to_string(),
                    grid_id: GridId::new("btc-core"),
                    batch_id: "batch-1".to_string(),
                    sequence: 0,
                    effect: GridEffect::NoOp,
                    status: EffectStatus::Pending,
                    attempt_count: 0,
                    last_error: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                }]),
            }
        }
    }

    #[async_trait::async_trait]
    impl StateRepositoryPort for PendingEffectPersistence {
        async fn save_transition(
            &self,
            id: &str,
            _state: &grid_engine::ports::GridSnapshot,
            _events: &[grid_core::events::DomainEvent],
            _effects: &[GridEffect],
        ) -> anyhow::Result<grid_engine::ports::CommittedGridWrite> {
            Ok(grid_engine::ports::CommittedGridWrite {
                grid_id: GridId::new(id),
                effects: Vec::new(),
            })
        }

        async fn load_grid_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::GridSnapshot>> {
            Ok(None)
        }

        async fn list_events(
            &self,
            _id: &str,
        ) -> anyhow::Result<Vec<grid_core::events::DomainEvent>> {
            Ok(Vec::new())
        }

        async fn list_pending_effects(&self) -> anyhow::Result<Vec<PersistedGridEffect>> {
            Ok(self
                .effects
                .lock()
                .unwrap()
                .iter()
                .filter(|effect| effect.status == EffectStatus::Pending)
                .cloned()
                .collect())
        }

        async fn mark_effect_executing(&self, _effect_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mark_effect_succeeded(&self, effect_id: &str) -> anyhow::Result<()> {
            let mut effects = self.effects.lock().unwrap();
            let effect = effects
                .iter_mut()
                .find(|effect| effect.effect_id == effect_id)
                .unwrap();
            effect.status = EffectStatus::Succeeded;
            effect.updated_at = Utc::now();
            Ok(())
        }

        async fn mark_effect_failed(&self, _effect_id: &str, _error: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct NoopExchange;

    #[async_trait::async_trait]
    impl grid_engine::ports::ExchangePort for NoopExchange {
        async fn submit_order(&self, _req: OrderRequest) -> anyhow::Result<OrderReceipt> {
            panic!("submit_order should not be called for noop effect")
        }

        async fn cancel_order(
            &self,
            _instrument: &Instrument,
            _order_id: &str,
        ) -> anyhow::Result<()> {
            panic!("cancel_order should not be called for noop effect")
        }

        async fn cancel_all(&self, _instrument: &Instrument) -> anyhow::Result<()> {
            panic!("cancel_all should not be called for noop effect")
        }

        async fn get_position(&self, _instrument: &Instrument) -> anyhow::Result<Position> {
            Ok(Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            })
        }

        async fn get_open_orders(
            &self,
            _instrument: &Instrument,
        ) -> anyhow::Result<Vec<ExchangeOrder>> {
            Ok(Vec::new())
        }

        async fn get_exchange_info(
            &self,
            _instrument: &Instrument,
        ) -> anyhow::Result<ExchangeInfo> {
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

        async fn get_server_time(&self) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
            Ok(Utc::now())
        }
    }

    struct FakeClock;

    impl grid_engine::ports::ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now()
        }
    }
}
