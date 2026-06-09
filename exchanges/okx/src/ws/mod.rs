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

use crate::{Credentials, instrument::OkxInstrumentRegistry};

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct OkxWsClient {
    public_ws_url: String,
    private_ws_url: String,
    credentials: Credentials,
    reconnect_delay: Duration,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    instrument_registry: Arc<OkxInstrumentRegistry>,
}

impl OkxWsClient {
    pub(crate) fn new_with_instrument_registry(
        public_ws_url: impl Into<String>,
        private_ws_url: impl Into<String>,
        credentials: Credentials,
        instrument_registry: Arc<OkxInstrumentRegistry>,
    ) -> Self {
        Self {
            public_ws_url: public_ws_url.into().trim_end_matches('/').to_string(),
            private_ws_url: private_ws_url.into().trim_end_matches('/').to_string(),
            credentials,
            reconnect_delay: Duration::from_millis(250),
            timestamp_provider: Arc::new(|| chrono::Utc::now().timestamp()),
            instrument_registry,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_params(
        public_ws_url: impl Into<String>,
        private_ws_url: impl Into<String>,
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        passphrase: impl Into<String>,
        reconnect_delay: Duration,
        timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            public_ws_url: public_ws_url.into().trim_end_matches('/').to_string(),
            private_ws_url: private_ws_url.into().trim_end_matches('/').to_string(),
            credentials: Credentials::new(api_key, api_secret, passphrase),
            reconnect_delay,
            timestamp_provider,
            instrument_registry: Arc::new(OkxInstrumentRegistry::default()),
        }
    }

    pub(crate) async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = self.public_ws_url.clone();
        let symbol = instrument.symbol.clone();
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            market::run_market_stream(url, symbol, sender, reconnect_delay).await;
        });

        Ok(receiver)
    }

    pub(crate) async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = self.private_ws_url.clone();
        let credentials = self.credentials.clone();
        let timestamp_provider = Arc::clone(&self.timestamp_provider);
        let reconnect_delay = self.reconnect_delay;
        let instrument_registry = Arc::clone(&self.instrument_registry);

        tokio::spawn(async move {
            account::run_user_stream(
                url,
                credentials,
                timestamp_provider,
                instrument_registry,
                sender,
                reconnect_delay,
            )
            .await;
        });

        Ok(receiver)
    }
}

async fn connect_websocket(
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

fn backoff_delay(base: Duration, attempt: u32) -> Duration {
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
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::TrackPnlRecordKind;
    use poise_engine::ports::{ExecutionQuote, MarketDataTick, OrderStatus, UserDataPayload};
    use tokio::{
        net::TcpListener,
        time::{Duration, timeout},
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::*;
    use crate::instrument::{OkxInstrumentMetadata, OkxInstrumentRegistry};
    use crate::rest::models::InstrumentInfo;

    #[test]
    fn tickers_message_maps_to_execution_quote() {
        let ticks = market::parse_market_message(
            "BTC-USDT-SWAP",
            r#"{"arg":{"channel":"tickers","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","bidPx":"63999.5","askPx":"64000.5","ts":"1700000000000"}]}"#,
        )
        .unwrap();

        assert_eq!(ticks.len(), 1);
        match &ticks[0] {
            MarketDataTick::ExecutionQuote(tick) => {
                assert_eq!(
                    tick.instrument,
                    Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
                );
                assert_eq!(
                    tick.execution_quote,
                    ExecutionQuote {
                        best_bid: 63999.5,
                        best_ask: 64000.5
                    }
                );
                assert_eq!(tick.timestamp.timestamp_millis(), 1_700_000_000_000);
            }
            other => panic!("expected quote tick, got {other:?}"),
        }
    }

    #[test]
    fn mark_price_message_maps_to_mark_tick() {
        let ticks = market::parse_market_message(
            "BTC-USDT-SWAP",
            r#"{"arg":{"channel":"mark-price","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","markPx":"64000.1","ts":"1700000000000"}]}"#,
        )
        .unwrap();

        assert_eq!(ticks.len(), 1);
        match &ticks[0] {
            MarketDataTick::MarkPrice(tick) => {
                assert_eq!(
                    tick.instrument,
                    Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
                );
                assert_eq!(tick.mark_price, 64000.1);
                assert_eq!(tick.timestamp.timestamp_millis(), 1_700_000_000_000);
            }
            other => panic!("expected mark tick, got {other:?}"),
        }
    }

    #[test]
    fn private_login_payload_contains_signature_fields() {
        let payload = account::build_login_payload(
            "api-key",
            "22582BD0CFF14C41EDBF1AB98506286D",
            "passphrase",
            1_704_876_947,
        );
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();

        assert_eq!(value["op"], "login");
        assert_eq!(value["args"][0]["apiKey"], "api-key");
        assert_eq!(value["args"][0]["passphrase"], "passphrase");
        assert_eq!(value["args"][0]["timestamp"], "1704876947");
        assert_eq!(
            value["args"][0]["sign"],
            "5/36BgGV6m/6pmdc20zdqk0mzF5ZalmzzPD2fo3wavU="
        );
    }

    #[test]
    fn orders_message_maps_to_order_update_and_trade_pnl() {
        let events = account::parse_user_data_message(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP","ordId":"123","clOrdId":"client-1","side":"buy","px":"64000.1","sz":"0.2","accFillSz":"0.05","state":"partially_filled","fillPx":"64000.0","fillSz":"0.05","fillPnl":"12.34","fee":"-1.25","feeCcy":"USDT","tradeId":"trade-1","uTime":"1700000000000"}]}"#,
        )
        .unwrap();

        assert_eq!(events.len(), 2);
        match &events[0].payload {
            UserDataPayload::OrderUpdate(order) => {
                assert_eq!(
                    order.instrument,
                    Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
                );
                assert_eq!(order.side, Side::Buy);
                assert_eq!(order.status, OrderStatus::PartiallyFilled);
            }
            other => panic!("expected order update, got {other:?}"),
        }
        match &events[1].payload {
            UserDataPayload::TrackPnl(record) => {
                assert_eq!(record.kind, TrackPnlRecordKind::Trade);
                assert_eq!(record.trade_id.as_deref(), Some("trade-1"));
                assert_eq!(record.realized_pnl, 12.34);
                assert_eq!(record.trading_fee, 1.25);
            }
            other => panic!("expected trade pnl, got {other:?}"),
        }
    }

    #[test]
    fn inverse_orders_message_uses_contract_qty_and_settlement_pnl_asset() {
        let registry = registry_with_metadata(inverse_metadata());
        let events = account::parse_user_data_message_with_registry(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"BTC-USD-SWAP","ordId":"123","clOrdId":"client-1","side":"sell","px":"64000.1","sz":"30","accFillSz":"10","state":"partially_filled","fillPx":"64000.0","fillSz":"10","fillPnl":"0.0012","fee":"-0.00001","feeCcy":"BTC","tradeId":"trade-1","uTime":"1700000000000"}]}"#,
            &registry,
        )
        .unwrap();

        match &events[0].payload {
            UserDataPayload::OrderUpdate(order) => {
                assert_eq!(
                    order.instrument,
                    Instrument::new(Venue::Okx, "BTC-USD-SWAP")
                );
                assert_eq!(order.qty, 30.0);
                assert_eq!(order.filled_qty, 10.0);
            }
            other => panic!("expected order update, got {other:?}"),
        }
        match &events[1].payload {
            UserDataPayload::TrackPnl(record) => {
                assert_eq!(record.pnl_asset, "BTC");
                assert_eq!(record.qty, Some(10.0));
                assert_eq!(record.realized_pnl, 0.0012);
                assert_eq!(record.trading_fee, 0.00001);
            }
            other => panic!("expected trade pnl, got {other:?}"),
        }
    }

    #[test]
    fn orders_message_rejects_fee_asset_mismatch_when_metadata_is_known() {
        let registry = registry_with_metadata(inverse_metadata());
        let error = account::parse_user_data_message_with_registry(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"BTC-USD-SWAP","ordId":"123","clOrdId":"client-1","side":"sell","px":"64000.1","sz":"30","accFillSz":"10","state":"partially_filled","fillPx":"64000.0","fillSz":"10","fillPnl":"0.0012","fee":"-0.00001","feeCcy":"USD","tradeId":"trade-1","uTime":"1700000000000"}]}"#,
            &registry,
        )
        .unwrap_err();

        assert!(error.to_string().contains("fee asset"));
    }

    #[test]
    fn orders_message_maps_okx_current_fill_pnl_and_fee_cost() {
        let events = account::parse_user_data_message(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"ANTHROPIC-USDT-SWAP","ordId":"123","clOrdId":"client-1","side":"sell","px":"1500.1","sz":"0.16","accFillSz":"0.16","state":"filled","fillPx":"1498.0","fillSz":"0.16","fillPnl":"-2.34","fillFee":"-0.12","fillFeeCcy":"USDT","tradeId":"trade-1","uTime":"1700000000000"}]}"#,
        )
        .unwrap();

        match &events[1].payload {
            UserDataPayload::TrackPnl(record) => {
                assert_eq!(record.realized_pnl, -2.34);
                assert_eq!(record.trading_fee, 0.12);
                assert_eq!(
                    record.source_key.as_deref(),
                    Some("okx:orders:anthropic-usdt-swap:trade-1")
                );
            }
            other => panic!("expected trade pnl, got {other:?}"),
        }
    }

    #[test]
    fn orders_message_accepts_market_fill_with_empty_order_price() {
        let events = account::parse_user_data_message(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"ANTHROPIC-USDT-SWAP","ordId":"123","clOrdId":"","side":"sell","px":"","sz":"0.06","accFillSz":"0.06","state":"filled","fillPx":"1666.6","fillSz":"0.06","fillPnl":"5.4390778138112309","fillFee":"-0.049998","fillFeeCcy":"USDT","tradeId":"445492","uTime":"1778456817451"}]}"#,
        )
        .unwrap();

        assert_eq!(events.len(), 2);
        match &events[0].payload {
            UserDataPayload::OrderUpdate(order) => {
                assert_eq!(order.client_order_id, "");
                assert_eq!(order.price, 1666.6);
                assert_eq!(order.status, OrderStatus::Filled);
            }
            other => panic!("expected order update, got {other:?}"),
        }
        match &events[1].payload {
            UserDataPayload::TrackPnl(record) => {
                assert_eq!(record.trade_id.as_deref(), Some("445492"));
                assert_eq!(record.price, Some(1666.6));
                assert_eq!(record.realized_pnl, 5.4390778138112309);
                assert_eq!(record.trading_fee, 0.049998);
            }
            other => panic!("expected trade pnl, got {other:?}"),
        }
    }

    #[test]
    fn orders_message_accepts_legacy_order_pnl_and_fee_fields() {
        let events = account::parse_user_data_message(
            r#"{"arg":{"channel":"orders","instType":"SWAP"},"data":[{"instId":"ANTHROPIC-USDT-SWAP","ordId":"123","clOrdId":"client-1","side":"sell","px":"1500.1","sz":"0.16","accFillSz":"0.16","state":"filled","fillPx":"1498.0","fillSz":"0.16","pnl":"-2.34","fee":"-0.12","feeCcy":"USDT","tradeId":"trade-1","uTime":"1700000000000"}]}"#,
        )
        .unwrap();

        match &events[1].payload {
            UserDataPayload::TrackPnl(record) => {
                assert_eq!(record.realized_pnl, -2.34);
                assert_eq!(record.trading_fee, 0.12);
            }
            other => panic!("expected trade pnl, got {other:?}"),
        }
    }

    #[test]
    fn positions_message_maps_to_position_update() {
        let events = account::parse_user_data_message(
            r#"{"arg":{"channel":"positions","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP","pos":"-0.25","avgPx":"65000.5","upl":"123.45","posSide":"net","lever":"20","uTime":"1700000000000"}]}"#,
        )
        .unwrap();

        assert_eq!(events.len(), 1);
        match &events[0].payload {
            UserDataPayload::PositionUpdate(position) => {
                assert_eq!(
                    position.instrument,
                    Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
                );
                assert_eq!(position.qty, -0.25);
            }
            other => panic!("expected position update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn public_ws_disconnect_reconnects_and_resubscribes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_server = Arc::clone(&observed);

        tokio::spawn(async move {
            for payload in [
                r#"{"arg":{"channel":"mark-price","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","markPx":"64000.1","ts":"1700000000000"}]}"#,
                r#"{"arg":{"channel":"mark-price","instId":"BTC-USDT-SWAP"},"data":[{"instId":"BTC-USDT-SWAP","markPx":"64010.2","ts":"1700000005000"}]}"#,
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                if let Some(Ok(Message::Text(text))) = websocket.next().await {
                    observed_server.lock().unwrap().push(text);
                }
                websocket
                    .send(Message::Text(payload.to_string()))
                    .await
                    .unwrap();
                websocket.close(None).await.unwrap();
            }
        });

        let client = OkxWsClient::with_test_params(
            format!("ws://{address}"),
            format!("ws://{address}"),
            "api-key",
            "secret-key",
            "passphrase",
            Duration::from_millis(10),
            Arc::new(|| 1_704_876_947),
        );
        let mut receiver = client
            .subscribe_prices(&Instrument::new(Venue::Okx, "BTC-USDT-SWAP"))
            .await
            .unwrap();
        let first = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(observed.lock().unwrap().len(), 2);
        assert!(observed.lock().unwrap()[0].contains("\"channel\":\"tickers\""));
        match first {
            MarketDataTick::MarkPrice(tick) => assert_eq!(tick.mark_price, 64000.1),
            _ => panic!("expected mark"),
        }
        match second {
            MarketDataTick::MarkPrice(tick) => assert_eq!(tick.mark_price, 64010.2),
            _ => panic!("expected mark"),
        }
    }

    #[tokio::test]
    async fn private_ws_disconnect_relogs_in_and_resubscribes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_server = Arc::clone(&observed);

        tokio::spawn(async move {
            for payload in [
                r#"{"arg":{"channel":"positions","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP","pos":"10","avgPx":"64000","upl":"1","posSide":"net","lever":"10","uTime":"1700000000000"}]}"#,
                r#"{"arg":{"channel":"positions","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP","pos":"20","avgPx":"64000","upl":"2","posSide":"net","lever":"10","uTime":"1700000005000"}]}"#,
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                for _ in 0..2 {
                    if let Some(Ok(Message::Text(text))) = websocket.next().await {
                        observed_server.lock().unwrap().push(text);
                        if observed_server.lock().unwrap().len() % 2 == 0 {
                            websocket
                                .send(Message::Text(payload.to_string()))
                                .await
                                .unwrap();
                            websocket.close(None).await.unwrap();
                            break;
                        }
                    }
                }
            }
        });

        let client = OkxWsClient::with_test_params(
            format!("ws://{address}"),
            format!("ws://{address}"),
            "api-key",
            "secret-key",
            "passphrase",
            Duration::from_millis(10),
            Arc::new(|| 1_704_876_947),
        );
        seed_linear_metadata(&client);
        let mut receiver = client.subscribe_user_data().await.unwrap();
        let first = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        let messages = observed.lock().unwrap();
        assert_eq!(messages.len(), 4);
        assert!(messages[0].contains("\"op\":\"login\""));
        assert!(messages[1].contains("\"op\":\"subscribe\""));
        assert!(messages[2].contains("\"op\":\"login\""));
        assert!(messages[3].contains("\"op\":\"subscribe\""));
        match first.payload {
            UserDataPayload::PositionUpdate(position) => assert_eq!(position.qty, 0.1),
            _ => panic!("expected position"),
        }
        match second.payload {
            UserDataPayload::PositionUpdate(position) => assert_eq!(position.qty, 0.2),
            _ => panic!("expected position"),
        }
    }

    fn seed_linear_metadata(client: &OkxWsClient) {
        client.instrument_registry.upsert(linear_metadata());
    }

    fn registry_with_metadata(metadata: OkxInstrumentMetadata) -> OkxInstrumentRegistry {
        let registry = OkxInstrumentRegistry::default();
        registry.upsert(metadata);
        registry
    }

    fn linear_metadata() -> OkxInstrumentMetadata {
        OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ct_type: Some("linear".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "0.01".to_string(),
            min_sz: "0.01".to_string(),
            ct_val: Some("0.01".to_string()),
            ct_val_ccy: Some("BTC".to_string()),
            settle_ccy: Some("USDT".to_string()),
        })
        .unwrap()
    }

    fn inverse_metadata() -> OkxInstrumentMetadata {
        OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
            inst_id: "BTC-USD-SWAP".to_string(),
            ct_type: Some("inverse".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "1".to_string(),
            min_sz: "1".to_string(),
            ct_val: Some("100".to_string()),
            ct_val_ccy: Some("USD".to_string()),
            settle_ccy: Some("BTC".to_string()),
        })
        .unwrap()
    }
}
