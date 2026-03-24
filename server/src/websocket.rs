use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use grid_core::events::DomainEvent;
use serde::{Deserialize, Serialize};

use crate::assembly::AppState;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsEvent {
    pub instance_id: String,
    pub event: DomainEvent,
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut receiver = state.events.subscribe();

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
    use std::time::Duration;

    use axum::Router;
    use futures_util::StreamExt;
    use grid_core::events::DomainEvent;
    use grid_core::types::Exposure;
    use tokio::net::TcpListener;
    use tokio_tungstenite::connect_async;

    use crate::assembly::AppState;

    use super::{WsEvent, ws_handler};

    async fn spawn_server() -> (String, tokio::sync::broadcast::Sender<WsEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (events, _) = tokio::sync::broadcast::channel(16);
        let state = AppState {
            manager: std::sync::Arc::new(tokio::sync::RwLock::new(
                grid_engine::manager::InstanceManager::new(
                    std::sync::Arc::new(FakeExchange),
                    std::sync::Arc::new(FakePersistence),
                    std::sync::Arc::new(FakeClock),
                ),
            )),
            persistence: std::sync::Arc::new(FakePersistence),
            mutation_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            events: events.clone(),
        };
        let app = Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("ws://{address}/ws"), events)
    }

    #[tokio::test]
    async fn broadcasts_events_to_multiple_clients() {
        let (url, events) = spawn_server().await;
        let (client_a, _) = connect_async(&url).await.unwrap();
        let (client_b, _) = connect_async(&url).await.unwrap();
        let (_, mut stream_a) = client_a.split();
        let (_, mut stream_b) = client_b.split();

        events
            .send(WsEvent {
                instance_id: "BTCUSDT".into(),
                event: DomainEvent::ExposureTargetChanged {
                    from: Exposure(0.0),
                    to: Exposure(4.0),
                },
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
        assert_eq!(payload_a.instance_id, "BTCUSDT");
    }

    struct FakeExchange;

    #[async_trait::async_trait]
    impl grid_engine::ports::ExchangePort for FakeExchange {
        async fn submit_order(
            &self,
            _req: grid_engine::ports::OrderRequest,
        ) -> anyhow::Result<grid_engine::ports::OrderReceipt> {
            unreachable!()
        }

        async fn cancel_order(&self, _symbol: &str, _order_id: &str) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn cancel_all(&self, _symbol: &str) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn get_position(
            &self,
            _symbol: &str,
        ) -> anyhow::Result<grid_engine::ports::Position> {
            unreachable!()
        }

        async fn get_open_orders(
            &self,
            _symbol: &str,
        ) -> anyhow::Result<Vec<grid_engine::ports::OpenOrder>> {
            unreachable!()
        }

        async fn get_exchange_info(
            &self,
            _symbol: &str,
        ) -> anyhow::Result<grid_engine::ports::ExchangeInfo> {
            unreachable!()
        }

        async fn get_server_time(&self) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
            unreachable!()
        }
    }

    struct FakePersistence;

    #[async_trait::async_trait]
    impl grid_engine::ports::PersistencePort for FakePersistence {
        async fn save_instance_state(
            &self,
            _id: &str,
            _state: &grid_engine::ports::InstanceSnapshot,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn load_instance_state(
            &self,
            _id: &str,
        ) -> anyhow::Result<Option<grid_engine::ports::InstanceSnapshot>> {
            Ok(None)
        }
    }

    struct FakeClock;

    impl grid_engine::ports::ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::Utc::now()
        }
    }
}
