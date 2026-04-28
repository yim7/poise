mod account;
mod market;
mod models;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::{net::TcpStream, sync::mpsc, time::Duration};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async, connect_async_tls_with_config,
    tungstenite::{Error as WebSocketError, error::ProtocolError},
};

use poise_core::track::Instrument;
use poise_engine::ports::{MarketDataTick, UserDataEvent};

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct BybitWsClient {
    public_ws_base_url: String,
    private_ws_base_url: String,
    api_key: String,
    api_secret: String,
    reconnect_delay: Duration,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl BybitWsClient {
    pub fn new(
        deployment: crate::Deployment,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
    ) -> Self {
        let (public_ws_base_url, private_ws_base_url) = deployment_ws_endpoints(deployment);
        Self {
            public_ws_base_url: public_ws_base_url.to_string(),
            private_ws_base_url: private_ws_base_url.to_string(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            reconnect_delay: Duration::from_millis(250),
            timestamp_provider: Arc::new(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_params(
        public_ws_base_url: impl Into<String>,
        private_ws_base_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        reconnect_delay: Duration,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            public_ws_base_url: public_ws_base_url.into().trim_end_matches('/').to_string(),
            private_ws_base_url: private_ws_base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            api_secret: api_secret.into(),
            reconnect_delay,
            timestamp_provider,
        }
    }

    pub async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = self.public_ws_base_url.clone();
        let symbol = instrument.symbol.clone();
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            market::run_market_stream(url, symbol, sender, reconnect_delay).await;
        });

        Ok(receiver)
    }

    pub async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = self.private_ws_base_url.clone();
        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();
        let timestamp_provider = Arc::clone(&self.timestamp_provider);
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            account::run_user_stream(
                url,
                api_key,
                api_secret,
                timestamp_provider,
                sender,
                reconnect_delay,
            )
            .await;
        });

        Ok(receiver)
    }
}

fn deployment_ws_endpoints(deployment: crate::Deployment) -> (&'static str, &'static str) {
    match deployment {
        crate::Deployment::Mainnet => (
            "wss://stream.bybit.com/v5/public/linear",
            "wss://stream.bybit.com/v5/private",
        ),
        crate::Deployment::Testnet => (
            "wss://stream-testnet.bybit.com/v5/public/linear",
            "wss://stream-testnet.bybit.com/v5/private",
        ),
    }
}

pub(crate) async fn connect_websocket(
    url: &str,
) -> Result<(
    WebSocket,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let connector = websocket_connector(url)?;
    let result = match connector {
        Some(connector) => connect_async_tls_with_config(url, None, false, Some(connector)).await,
        None => connect_async(url).await,
    };

    result.with_context(|| format!("failed to connect websocket `{url}`"))
}

pub(crate) fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(4)).unwrap_or(16);
    base.saturating_mul(multiplier)
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

#[cfg(test)]
mod tests {
    use tokio_tungstenite::tungstenite::{Error as WebSocketError, error::ProtocolError};

    #[test]
    fn treats_reset_without_close_handshake_as_expected_disconnect() {
        let error = WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake);

        assert!(super::is_expected_disconnect(&error));
    }

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
}
