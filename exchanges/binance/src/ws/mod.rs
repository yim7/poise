mod account;
mod market;
mod models;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async, connect_async_tls_with_config,
    tungstenite::{Error as WebSocketError, error::ProtocolError},
};

use poise_engine::ports::{MarketDataTick, UserDataEvent};
use poise_engine::track::Instrument;

use crate::rest::BinanceRestClient;

type UserWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct BinanceWsClient {
    rest: Arc<BinanceRestClient>,
    ws_base_url: String,
    reconnect_delay: Duration,
}

impl BinanceWsClient {
    pub fn new(rest: Arc<BinanceRestClient>, ws_base_url: impl Into<String>) -> Self {
        Self {
            rest,
            ws_base_url: ws_base_url.into().trim_end_matches('/').to_string(),
            reconnect_delay: Duration::from_millis(250),
        }
    }

    #[cfg(test)]
    fn with_reconnect_delay(
        rest: Arc<BinanceRestClient>,
        ws_base_url: impl Into<String>,
        reconnect_delay: Duration,
    ) -> Self {
        Self {
            rest,
            ws_base_url: ws_base_url.into().trim_end_matches('/').to_string(),
            reconnect_delay,
        }
    }

    pub async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = format!(
            "{}/stream?streams={}@markPrice/{}@bookTicker",
            self.ws_base_url,
            instrument.symbol.to_lowercase(),
            instrument.symbol.to_lowercase()
        );
        let symbol = instrument.symbol.clone();
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            market::run_market_stream(url, symbol, sender, reconnect_delay).await;
        });

        Ok(receiver)
    }

    pub async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (sender, receiver) = mpsc::channel(128);
        let ws_base_url = self.ws_base_url.clone();
        let rest = Arc::clone(&self.rest);
        let reconnect_delay = self.reconnect_delay;
        let initial_listen_key = rest.start_user_stream().await?;
        let initial_websocket = connect_user_stream(&ws_base_url, &initial_listen_key).await?;

        tokio::spawn(async move {
            account::run_user_stream(
                ws_base_url,
                rest,
                initial_listen_key,
                Some(initial_websocket),
                sender,
                reconnect_delay,
            )
            .await;
        });

        Ok(receiver)
    }
}

async fn connect_user_stream(ws_base_url: &str, listen_key: &str) -> Result<UserWebSocket> {
    let url = format!("{ws_base_url}/ws/{listen_key}");
    let (websocket, _) = connect_websocket(&url)
        .await
        .with_context(|| format!("failed to connect user data websocket `{url}`"))?;
    Ok(websocket)
}

async fn connect_websocket(
    url: &str,
) -> Result<(
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let connector = websocket_connector(url)?;
    let result = match connector {
        Some(connector) => connect_async_tls_with_config(url, None, false, Some(connector)).await,
        None => connect_async(url).await,
    };

    result.with_context(|| format!("failed to connect websocket `{url}`"))
}

fn websocket_connector(url: &str) -> Result<Option<Connector>> {
    if !url.starts_with("wss://") {
        return Ok(None);
    }

    let connector = native_tls::TlsConnector::builder()
        .build()
        .context("failed to build native TLS websocket connector")?;

    Ok(Some(Connector::NativeTls(connector)))
}

fn log_websocket_error(stream_name: &str, error: &WebSocketError) {
    if is_expected_disconnect(error) {
        tracing::info!("{stream_name} websocket disconnected: {error}; reconnecting");
    } else {
        tracing::warn!("{stream_name} websocket error: {error}");
    }
}

fn is_expected_disconnect(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::ConnectionClosed
            | WebSocketError::AlreadyClosed
            | WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepaliveStatus {
    Ok,
    Failed,
}

impl KeepaliveStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct KeepaliveObservation {
    finished_at: Instant,
    latency: Duration,
    status: KeepaliveStatus,
}

#[derive(Debug, Clone)]
struct UserStreamDiagnostics {
    connected_at: Instant,
    last_message_at: Instant,
    last_keepalive: Option<KeepaliveObservation>,
    last_send_wait: Option<Duration>,
    max_send_wait: Duration,
}

impl UserStreamDiagnostics {
    fn new(now: Instant) -> Self {
        Self {
            connected_at: now,
            last_message_at: now,
            last_keepalive: None,
            last_send_wait: None,
            max_send_wait: Duration::ZERO,
        }
    }

    fn record_message(&mut self, now: Instant) {
        self.last_message_at = now;
    }

    fn record_send_wait(&mut self, wait: Duration) {
        self.last_send_wait = Some(wait);
        if wait > self.max_send_wait {
            self.max_send_wait = wait;
        }
    }

    fn record_keepalive_result(
        &mut self,
        started_at: Instant,
        finished_at: Instant,
        status: KeepaliveStatus,
    ) {
        self.last_keepalive = Some(KeepaliveObservation {
            finished_at,
            latency: finished_at.saturating_duration_since(started_at),
            status,
        });
    }

    fn disconnect_snapshot(&self, now: Instant) -> UserStreamDisconnectSnapshot {
        UserStreamDisconnectSnapshot {
            connection_age: now.saturating_duration_since(self.connected_at),
            idle_for: now.saturating_duration_since(self.last_message_at),
            last_keepalive_age: self
                .last_keepalive
                .map(|observation| now.saturating_duration_since(observation.finished_at)),
            last_keepalive_latency: self.last_keepalive.map(|observation| observation.latency),
            last_keepalive_status: self.last_keepalive.map(|observation| observation.status),
            last_send_wait: self.last_send_wait,
            max_send_wait: self.max_send_wait,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UserStreamDisconnectSnapshot {
    connection_age: Duration,
    idle_for: Duration,
    last_keepalive_age: Option<Duration>,
    last_keepalive_latency: Option<Duration>,
    last_keepalive_status: Option<KeepaliveStatus>,
    last_send_wait: Option<Duration>,
    max_send_wait: Duration,
}

fn log_user_stream_disconnect(reason: &str, snapshot: UserStreamDisconnectSnapshot) {
    tracing::info!(
        reason,
        connection_age = ?snapshot.connection_age,
        idle_for = ?snapshot.idle_for,
        last_keepalive_age = ?snapshot.last_keepalive_age,
        last_keepalive_latency = ?snapshot.last_keepalive_latency,
        last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
        last_send_wait = ?snapshot.last_send_wait,
        max_send_wait = ?snapshot.max_send_wait,
        "user data websocket disconnected; reconnecting"
    );
}

fn log_user_stream_error(error: &WebSocketError, snapshot: UserStreamDisconnectSnapshot) {
    if is_expected_disconnect(error) {
        tracing::info!(
            error = %error,
            connection_age = ?snapshot.connection_age,
            idle_for = ?snapshot.idle_for,
            last_keepalive_age = ?snapshot.last_keepalive_age,
            last_keepalive_latency = ?snapshot.last_keepalive_latency,
            last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
            last_send_wait = ?snapshot.last_send_wait,
            max_send_wait = ?snapshot.max_send_wait,
            "user data websocket disconnected; reconnecting"
        );
    } else {
        tracing::warn!(
            error = %error,
            connection_age = ?snapshot.connection_age,
            idle_for = ?snapshot.idle_for,
            last_keepalive_age = ?snapshot.last_keepalive_age,
            last_keepalive_latency = ?snapshot.last_keepalive_latency,
            last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
            last_send_wait = ?snapshot.last_send_wait,
            max_send_wait = ?snapshot.max_send_wait,
            "user data websocket error"
        );
    }
}

fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(4)).unwrap_or(16);
    base.saturating_mul(multiplier)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn parse_side(value: &str) -> Result<poise_core::types::Side> {
    match value {
        "BUY" => Ok(poise_core::types::Side::Buy),
        "SELL" => Ok(poise_core::types::Side::Sell),
        other => Err(anyhow!("unsupported side: {other}")),
    }
}

fn quote_asset_for_symbol(symbol: &str) -> Option<&'static str> {
    ["USDT", "USDC", "FDUSD", "BUSD"]
        .into_iter()
        .find(|quote| symbol.ends_with(quote))
}

#[cfg(test)]
mod tests {
    use tokio::time::{Duration, Instant};
    use tokio_tungstenite::tungstenite::{Error as WebSocketError, error::ProtocolError};

    #[test]
    fn websocket_connector_uses_native_tls_for_secure_urls() {
        let connector = super::websocket_connector("wss://example.com/ws").unwrap();

        assert!(matches!(
            connector,
            Some(tokio_tungstenite::Connector::NativeTls(_))
        ));
    }

    #[test]
    fn websocket_connector_skips_tls_for_plain_urls() {
        let connector = super::websocket_connector("ws://127.0.0.1:18081/ws").unwrap();

        assert!(connector.is_none());
    }

    #[test]
    fn treats_reset_without_close_handshake_as_expected_disconnect() {
        let error = WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake);

        assert!(super::is_expected_disconnect(&error));
    }

    #[test]
    fn user_stream_diagnostics_snapshot_tracks_keepalive_and_send_wait() {
        let base = Instant::now();
        let mut diagnostics = super::UserStreamDiagnostics::new(base);

        diagnostics.record_message(base + Duration::from_secs(5));
        diagnostics.record_send_wait(Duration::from_millis(250));
        diagnostics.record_send_wait(Duration::from_millis(100));
        diagnostics.record_keepalive_result(
            base + Duration::from_secs(6),
            base + Duration::from_secs(8),
            super::KeepaliveStatus::Ok,
        );

        let snapshot = diagnostics.disconnect_snapshot(base + Duration::from_secs(20));

        assert_eq!(
            snapshot,
            super::UserStreamDisconnectSnapshot {
                connection_age: Duration::from_secs(20),
                idle_for: Duration::from_secs(15),
                last_keepalive_age: Some(Duration::from_secs(12)),
                last_keepalive_latency: Some(Duration::from_secs(2)),
                last_keepalive_status: Some(super::KeepaliveStatus::Ok),
                last_send_wait: Some(Duration::from_millis(100)),
                max_send_wait: Duration::from_millis(250),
            }
        );
    }
}
