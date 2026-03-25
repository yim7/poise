use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use url::{Host, Url};

use crate::protocol::{
    CommandRequest, CommandResponse, GridSnapshot, GridSummary, WsEvent,
};

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5));
        if should_bypass_proxy(&base_url) {
            builder = builder.no_proxy();
        }

        Self {
            base_url,
            http: builder.build().expect("failed to build reqwest client"),
        }
    }

    pub async fn list_instances(&self) -> Result<Vec<GridSummary>> {
        let response = self
            .http
            .get(self.endpoint("/grids"))
            .send()
            .await
            .context("failed to request instance list")?;

        decode_json(response, "list instances").await
    }

    pub async fn get_snapshot(&self, id: &str) -> Result<GridSnapshot> {
        let response = self
            .http
            .get(self.endpoint(&format!("/grids/{id}/snapshot")))
            .send()
            .await
            .with_context(|| format!("failed to request snapshot for `{id}`"))?;

        decode_json(response, "get snapshot").await
    }

    pub async fn submit_command(&self, id: &str, cmd: &str) -> Result<CommandResponse> {
        let response = self
            .http
            .post(self.endpoint(&format!("/grids/{id}/commands")))
            .json(&CommandRequest {
                command: cmd.to_string(),
            })
            .send()
            .await
            .with_context(|| format!("failed to submit `{cmd}` for `{id}`"))?;

        decode_json(response, "submit command").await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

fn should_bypass_proxy(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };

    match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(host)) => IpAddr::V4(host).is_loopback(),
        Some(Host::Ipv6(host)) => IpAddr::V6(host).is_loopback(),
        None => false,
    }
}

pub async fn connect_ws(url: &str) -> Result<mpsc::Receiver<WsEvent>> {
    let (stream, _) = connect_async(url)
        .await
        .with_context(|| format!("failed to connect websocket `{url}`"))?;
    let (_, mut read) = stream.split();
    let (sender, receiver) = mpsc::channel(64);

    tokio::spawn(async move {
        while let Some(message) = read.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let Ok(event) = serde_json::from_str::<WsEvent>(&text) else {
                        continue;
                    };
                    if sender.send(event).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    let Ok(text) = String::from_utf8(bytes.to_vec()) else {
                        continue;
                    };
                    let Ok(event) = serde_json::from_str::<WsEvent>(&text) else {
                        continue;
                    };
                    if sender.send(event).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    Ok(receiver)
}

async fn decode_json<T>(response: reqwest::Response, action: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read response body")?;

    if !status.is_success() {
        bail!("{action} failed with status {status}: {body}");
    }

    serde_json::from_str(&body).with_context(|| format!("failed to decode {action} response"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use crate::protocol::{
        CommandResponse, DomainEvent, GridConfig, GridSnapshot, GridStatus,
        GridSummary, OutOfBandPolicy, ShapeFamily, WsEvent,
    };

    use super::{ApiClient, connect_ws, should_bypass_proxy};

    #[derive(Clone)]
    struct StubState {
        snapshots: Arc<Mutex<HashMap<String, GridSnapshot>>>,
    }

    async fn list_instances(State(state): State<StubState>) -> Json<Vec<GridSummary>> {
        let snapshots = state.snapshots.lock().await;
        Json(
            snapshots
                .values()
                .map(|snapshot| GridSummary {
                    id: snapshot.id.clone(),
                    symbol: snapshot.symbol.clone(),
                    status: snapshot.status.clone(),
                    reference_price: snapshot.reference_price,
                })
                .collect(),
        )
    }

    async fn get_snapshot(
        axum::extract::Path(id): axum::extract::Path<String>,
        State(state): State<StubState>,
    ) -> Json<GridSnapshot> {
        Json(state.snapshots.lock().await.get(&id).unwrap().clone())
    }

    async fn submit_command(
        axum::extract::Path(id): axum::extract::Path<String>,
        State(state): State<StubState>,
        Json(command): Json<crate::protocol::CommandRequest>,
    ) -> Json<CommandResponse> {
        let mut snapshots = state.snapshots.lock().await;
        if let Some(snapshot) = snapshots.get_mut(&id) {
            if command.command == "pause" {
                snapshot.status = GridStatus::Paused;
            } else if command.command == "resume" {
                snapshot.status = GridStatus::Active;
            }
        }

        Json(CommandResponse {
            grid_id: id,
            command: command.command,
            accepted: true,
        })
    }

    async fn ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_socket)
    }

    async fn handle_socket(mut socket: WebSocket) {
        let payload = serde_json::to_string(&WsEvent {
            grid_id: "BTCUSDT".into(),
            event: DomainEvent::BandReentered { price: 99.0 },
        })
        .unwrap();
        socket.send(AxumMessage::Text(payload)).await.unwrap();
    }

    async fn spawn_stub_server() -> (String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = StubState {
            snapshots: Arc::new(Mutex::new(HashMap::from([(
                "BTCUSDT".to_string(),
                GridSnapshot {
                    id: "BTCUSDT".into(),
                    symbol: "BTCUSDT".into(),
                    status: GridStatus::Active,
                    current_exposure: 2.5,
                    target_exposure: None,
                    reference_price: Some(100.0),
                    pending_order: None,
                    config: GridConfig {
                        lower_price: 90.0,
                        upper_price: 110.0,
                        long_exposure_units: 8.0,
                        short_exposure_units: 8.0,
                        notional_per_unit: 375.0,
                        shape_family: ShapeFamily::Linear,
                        out_of_band_policy: OutOfBandPolicy::Freeze,
                    },
                },
            )]))),
        };
        let app = Router::new()
            .route("/grids", get(list_instances))
            .route("/grids/:id/snapshot", get(get_snapshot))
            .route("/grids/:id/commands", post(submit_command))
            .route("/ws", get(ws_handler))
            .with_state(state);

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{address}"), format!("ws://{address}/ws"))
    }

    #[tokio::test]
    async fn lists_instances_from_http() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let items = client.list_instances().await.unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "BTCUSDT");
    }

    #[tokio::test]
    async fn gets_snapshot_from_http() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let snapshot = client.get_snapshot("BTCUSDT").await.unwrap();

        assert_eq!(snapshot.current_exposure, 2.5);
        assert_eq!(snapshot.status, GridStatus::Active);
    }

    #[tokio::test]
    async fn submits_commands_over_http() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let response = client.submit_command("BTCUSDT", "pause").await.unwrap();
        let snapshot = client.get_snapshot("BTCUSDT").await.unwrap();

        assert!(response.accepted);
        assert_eq!(snapshot.status, GridStatus::Paused);
    }

    #[test]
    fn bypasses_proxy_for_loopback_hosts() {
        assert!(should_bypass_proxy("http://127.0.0.1:8000"));
        assert!(should_bypass_proxy("http://localhost:8000"));
        assert!(should_bypass_proxy("https://[::1]:9443"));
        assert!(!should_bypass_proxy("https://example.com"));
    }

    #[tokio::test]
    async fn receives_events_from_websocket() {
        let (_, ws_url) = spawn_stub_server().await;
        let mut receiver = connect_ws(&ws_url).await.unwrap();

        let event = receiver.recv().await.unwrap();

        assert_eq!(event.grid_id, "BTCUSDT");
        assert_eq!(event.event, DomainEvent::BandReentered { price: 99.0 });
    }
}
