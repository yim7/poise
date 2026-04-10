use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::PriceTick;
use poise_engine::track::{Instrument, Venue};

use super::{backoff_delay, connect_websocket, models::PublicTickerMessage};

pub(super) async fn run_market_stream(
    url: String,
    symbol: String,
    sender: mpsc::Sender<PriceTick>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_websocket(&url).await {
            Ok((mut websocket, _)) => {
                attempt = 0;
                if let Err(error) = subscribe(&mut websocket, &symbol).await {
                    tracing::warn!("failed to subscribe market stream: {error}");
                } else {
                    while let Some(message) = websocket.next().await {
                        match message {
                            Ok(Message::Text(text)) => match parse_linear_ticker_message(&text) {
                                Ok(Some(tick)) => {
                                    if sender.send(tick).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::warn!("failed to parse market data message: {error}");
                                }
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(error) => {
                                super::log_websocket_error("market data", &error);
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect market data websocket: {error}");
            }
        }

        if sender.is_closed() {
            return;
        }

        sleep(backoff_delay(reconnect_delay, attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

async fn subscribe(websocket: &mut super::WebSocket, symbol: &str) -> Result<()> {
    let payload = serde_json::json!({
        "op": "subscribe",
        "args": [format!("tickers.{symbol}")]
    });
    websocket
        .send(Message::Text(payload.to_string()))
        .await
        .with_context(|| format!("failed to subscribe market stream `{symbol}`"))?;
    Ok(())
}

pub(super) fn parse_linear_ticker_message(payload: &str) -> Result<Option<PriceTick>> {
    let message: PublicTickerMessage = serde_json::from_str(payload)?;
    if !message.topic.starts_with("tickers.") {
        return Ok(None);
    }

    let mark_price = parse_decimal("data.markPrice", &message.data.mark_price)?;
    let timestamp = Utc
        .timestamp_millis_opt(message.ts)
        .single()
        .context("invalid ticker timestamp")?;

    Ok(Some(PriceTick {
        instrument: Instrument::new(Venue::Bybit, message.data.symbol),
        reference_price: mark_price,
        mark_price,
        timestamp,
    }))
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_engine::track::{Instrument, Venue};

    use super::*;
    use crate::ws::BybitWsClient;

    #[test]
    fn parses_linear_ticker_message_into_price_tick() {
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000000000,
            "data": {
                "symbol": "BTCUSDT",
                "markPrice": "64000.10",
                "indexPrice": "63999.90"
            }
        }"#;

        let tick = parse_linear_ticker_message(payload).unwrap().unwrap();

        assert_eq!(
            tick,
            PriceTick {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                reference_price: 64000.10,
                mark_price: 64000.10,
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn reconnects_market_stream_after_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_server = Arc::clone(&observed);

        tokio::spawn(async move {
            for payload in [
                r#"{"topic":"tickers.BTCUSDT","ts":1700000000000,"data":{"symbol":"BTCUSDT","markPrice":"64000.10","indexPrice":"63999.90"}}"#,
                r#"{"topic":"tickers.BTCUSDT","ts":1700000005000,"data":{"symbol":"BTCUSDT","markPrice":"64010.20","indexPrice":"64010.00"}}"#,
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

        let client = BybitWsClient::with_test_params(
            format!("ws://{address}"),
            format!("ws://{address}"),
            "api-key",
            "secret-key",
            Duration::from_millis(10),
            Arc::new(|| 1_700_000_000_000),
        );

        let mut receiver = client
            .subscribe_prices(&Instrument::new(Venue::Bybit, "BTCUSDT"))
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

        let messages = observed.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"op\":\"subscribe\""));
        assert_eq!(first.mark_price, 64000.10);
        assert_eq!(second.mark_price, 64010.20);
    }
}
