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
    GridCommandType, TrackCommandAccepted, TrackCommandRequest, TrackDetailView,
    TrackDiagnosticsView, TrackListResponse, TrackStreamEvent,
};

#[derive(Debug, Clone)]
pub struct ApiClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug)]
enum WsMessageOutcome {
    Event(TrackStreamEvent),
    Closed,
    Ignore,
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

    pub async fn list_tracks(&self) -> Result<TrackListResponse> {
        let response = self
            .http
            .get(self.endpoint("/tracks"))
            .send()
            .await
            .context("failed to request track list")?;

        decode_json(response, "list tracks").await
    }

    pub async fn get_track_detail(&self, id: &str) -> Result<TrackDetailView> {
        let response = self
            .http
            .get(self.endpoint(&format!("/tracks/{id}")))
            .send()
            .await
            .with_context(|| format!("failed to request track detail for `{id}`"))?;

        decode_json(response, "get track detail").await
    }

    pub async fn get_track_diagnostics(&self, id: &str) -> Result<TrackDiagnosticsView> {
        let response = self
            .http
            .get(self.endpoint(&format!("/debug/tracks/{id}/diagnostics")))
            .send()
            .await
            .with_context(|| format!("failed to request track diagnostics for `{id}`"))?;

        decode_json(response, "get track diagnostics").await
    }

    pub async fn submit_command(
        &self,
        id: &str,
        cmd: GridCommandType,
    ) -> Result<TrackCommandAccepted> {
        let response = self
            .http
            .post(self.endpoint(&format!("/tracks/{id}/commands")))
            .json(&TrackCommandRequest { command: cmd })
            .send()
            .await
            .with_context(|| format!("failed to submit `{:?}` for `{id}`", cmd))?;

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

pub async fn connect_ws(url: &str) -> Result<mpsc::Receiver<TrackStreamEvent>> {
    let (stream, _) = connect_async(url)
        .await
        .with_context(|| format!("failed to connect websocket `{url}`"))?;
    let (_, mut read) = stream.split();
    let (sender, receiver) = mpsc::channel(64);

    tokio::spawn(async move {
        while let Some(message) = read.next().await {
            match message {
                Ok(message) => match decode_ws_message(message) {
                    Ok(WsMessageOutcome::Event(event)) => {
                        if sender.send(event).await.is_err() {
                            break;
                        }
                    }
                    Ok(WsMessageOutcome::Closed) => break,
                    Ok(WsMessageOutcome::Ignore) => {}
                    Err(error) => tracing::warn!("discard invalid websocket payload: {error}"),
                },
                Err(error) => {
                    tracing::warn!("websocket stream read failed: {error}");
                    break;
                }
            }
        }
    });

    Ok(receiver)
}

fn decode_ws_message(message: Message) -> Result<WsMessageOutcome> {
    match message {
        Message::Text(text) => Ok(WsMessageOutcome::Event(decode_ws_event_text(&text)?)),
        Message::Binary(bytes) => {
            let text = String::from_utf8(bytes.to_vec())
                .context("websocket message was not valid UTF-8")?;
            Ok(WsMessageOutcome::Event(decode_ws_event_text(&text)?))
        }
        Message::Close(_) => Ok(WsMessageOutcome::Closed),
        _ => Ok(WsMessageOutcome::Ignore),
    }
}

fn decode_ws_event_text(text: &str) -> Result<TrackStreamEvent> {
    serde_json::from_str(text).context("invalid websocket event json")
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
    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use tokio::net::TcpListener;

    use crate::protocol::{
        GridCommandType, TrackCommandAccepted, TrackCommandRequest, TrackDetailView,
        TrackDiagnosticsView, TrackListResponse, TrackStreamEvent,
    };

    use super::{ApiClient, connect_ws, should_bypass_proxy};

    const BTC_GRID_ID: &str = "btc-core";

    fn track_list_response() -> TrackListResponse {
        serde_json::from_str(include_str!("../tests/fixtures/track_list_response.json")).unwrap()
    }

    fn track_detail_view() -> TrackDetailView {
        serde_json::from_str(include_str!("../tests/fixtures/track_detail_view.json")).unwrap()
    }

    fn track_stream_event() -> TrackStreamEvent {
        serde_json::from_str(include_str!(
            "../tests/fixtures/ws_track_detail_changed.json"
        ))
        .unwrap()
    }

    fn track_diagnostics_view() -> TrackDiagnosticsView {
        serde_json::from_str(include_str!(
            "../tests/fixtures/track_diagnostics_view.json"
        ))
        .unwrap()
    }

    async fn list_tracks() -> Json<TrackListResponse> {
        Json(track_list_response())
    }

    async fn get_track_detail(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> Json<TrackDetailView> {
        let detail = track_detail_view();
        assert_eq!(detail.identity.id, id);
        Json(detail)
    }

    async fn get_track_diagnostics(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> Json<TrackDiagnosticsView> {
        assert_eq!(id, BTC_GRID_ID);
        Json(track_diagnostics_view())
    }

    async fn submit_command(
        axum::extract::Path(id): axum::extract::Path<String>,
        Json(command): Json<TrackCommandRequest>,
    ) -> Json<TrackCommandAccepted> {
        Json(TrackCommandAccepted {
            track_id: id,
            command: command.command,
            accepted: true,
        })
    }

    async fn ws_handler(ws: WebSocketUpgrade) -> Response {
        ws.on_upgrade(handle_socket)
    }

    async fn handle_socket(mut socket: WebSocket) {
        let payload = serde_json::to_string(&track_stream_event()).unwrap();
        socket.send(AxumMessage::Text(payload)).await.unwrap();
    }

    async fn spawn_stub_server() -> (String, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/tracks", get(list_tracks))
            .route("/tracks/:id", get(get_track_detail))
            .route("/debug/tracks/:id/diagnostics", get(get_track_diagnostics))
            .route("/tracks/:id/commands", post(submit_command))
            .route("/ws", get(ws_handler));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{address}"), format!("ws://{address}/ws"))
    }

    #[tokio::test]
    async fn list_tracks_decodes_track_list_response() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let response = client.list_tracks().await.unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, BTC_GRID_ID);
        assert_eq!(response.items[0].instrument.symbol, "BTCUSDT");
    }

    #[tokio::test]
    async fn get_track_detail_decodes_projected_detail() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let detail = client.get_track_detail(BTC_GRID_ID).await.unwrap();

        assert_eq!(detail.identity.id, BTC_GRID_ID);
        assert_eq!(detail.position.current_exposure, 3.5);
        assert!((detail.statistics.realized_pnl - 980.1).abs() < f64::EPSILON);
        assert!((detail.statistics.total_pnl - 1245.3).abs() < f64::EPSILON);
        assert_eq!(detail.available_commands[0].command, GridCommandType::Pause);
    }

    #[tokio::test]
    async fn get_track_diagnostics_decodes_debug_payload() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let diagnostics = client.get_track_diagnostics(BTC_GRID_ID).await.unwrap();

        assert_eq!(diagnostics.items.len(), 1);
        assert!(diagnostics.items[0].message.contains("target exposure"));
    }

    #[tokio::test]
    async fn submits_typed_commands_over_http() {
        let (base_url, _) = spawn_stub_server().await;
        let client = ApiClient::new(base_url);

        let response = client
            .submit_command(BTC_GRID_ID, GridCommandType::Pause)
            .await
            .unwrap();

        assert!(response.accepted);
        assert_eq!(response.track_id, BTC_GRID_ID);
        assert_eq!(response.command, GridCommandType::Pause);
    }

    #[test]
    fn bypasses_proxy_for_loopback_hosts() {
        assert!(should_bypass_proxy("http://127.0.0.1:8000"));
        assert!(should_bypass_proxy("http://localhost:8000"));
        assert!(should_bypass_proxy("https://[::1]:9443"));
        assert!(!should_bypass_proxy("https://example.com"));
    }

    #[tokio::test]
    async fn receives_track_stream_events_from_websocket() {
        let (_, ws_url) = spawn_stub_server().await;
        let mut receiver = connect_ws(&ws_url).await.unwrap();

        let event = receiver.recv().await.unwrap();

        assert_eq!(event.track_id, BTC_GRID_ID);
        assert_eq!(event.payload, track_stream_event().payload);
    }

    #[test]
    fn decode_ws_message_rejects_invalid_json_text() {
        let error = super::decode_ws_message(tokio_tungstenite::tungstenite::Message::Text(
            "{not-json".into(),
        ))
        .unwrap_err();

        assert!(error.to_string().contains("invalid websocket event json"));
    }
}
