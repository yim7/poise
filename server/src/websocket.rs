use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
#[cfg_attr(not(test), allow(dead_code))]
pub type WsEvent = grid_protocol::WsEvent;

use crate::assembly::ServerState;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    let mut receiver = state.service.subscribe_events();

    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
    use std::time::Duration;

    use axum::Router;
    use futures_util::StreamExt;
    use grid_core::risk::CapacityBudget;
    use grid_core::strategy::{GridConfig, OutOfBandPolicy, ShapeFamily};
    use grid_core::types::ExchangeRules;
    use grid_engine::command::GridCommand;
    use grid_engine::grid::{GridId, Instrument, Venue};
    use grid_protocol::DomainEvent;
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::application::GridPlatformService;
    use crate::assembly::ServerState;

    use super::{WsEvent, ws_handler};

    async fn spawn_server() -> (
        String,
        Arc<GridPlatformService>,
        tokio::sync::broadcast::Sender<WsEvent>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
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
        let service = Arc::new(GridPlatformService::new(
            manager,
            std::sync::Arc::new(FakePersistence),
            events.clone(),
        ));
        let state = ServerState {
            service: Arc::clone(&service),
        };
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("ws://{address}/ws"), service, events)
    }

    #[tokio::test]
    async fn broadcasts_events_to_multiple_clients() {
        let (url, _service, events) = spawn_server().await;
        let (client_a, _) = connect_async(&url).await.unwrap();
        let (client_b, _) = connect_async(&url).await.unwrap();
        let (_, mut stream_a) = client_a.split();
        let (_, mut stream_b) = client_b.split();

        events
            .send(WsEvent {
                grid_id: "BTCUSDT".into(),
                event: DomainEvent::ExposureTargetChanged { from: 0.0, to: 4.0 },
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
        assert_eq!(payload_a.grid_id, "BTCUSDT");
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

    struct FakeClock;

    impl grid_engine::ports::ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now()
        }
    }
}
