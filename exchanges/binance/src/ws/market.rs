use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::PriceTick;
use poise_engine::track::{Instrument, Venue};

use super::{
    backoff_delay, connect_websocket, log_websocket_error, models::MarkPriceMessage, parse_decimal,
};

pub(super) async fn run_market_stream(
    url: String,
    sender: mpsc::Sender<PriceTick>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_websocket(&url).await {
            Ok((mut websocket, _)) => {
                attempt = 0;

                while let Some(message) = websocket.next().await {
                    match message {
                        Ok(Message::Text(text)) => match parse_mark_price_message(&text) {
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
                            log_websocket_error("market data", &error);
                            break;
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

pub(super) fn parse_mark_price_message(payload: &str) -> Result<Option<PriceTick>> {
    let message: MarkPriceMessage = serde_json::from_str(payload)?;
    let mark_price = parse_decimal("p", &message.mark_price)?;
    let timestamp = Utc
        .timestamp_millis_opt(message.event_time)
        .single()
        .context("invalid event timestamp")?;

    Ok(Some(PriceTick {
        instrument: Instrument::new(Venue::Binance, message.symbol),
        reference_price: mark_price,
        mark_price,
        timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};
    use futures_util::SinkExt;
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_engine::track::{Instrument, Venue};

    use super::*;
    use crate::rest::BinanceRestClient;
    use crate::ws::BinanceWsClient;

    #[test]
    fn parses_mark_price_stream_message() {
        let payload = r#"{
            "e": "markPriceUpdate",
            "E": 1700000000000,
            "s": "BTCUSDT",
            "p": "64000.10",
            "i": "63999.90"
        }"#;

        let tick = parse_mark_price_message(payload).unwrap().unwrap();

        assert_eq!(
            tick,
            PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 64000.10,
                mark_price: 64000.10,
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn reconnects_market_price_stream_after_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for payload in [
                r#"{"e":"markPriceUpdate","E":1700000000000,"s":"BTCUSDT","p":"64000.10","i":"63999.90"}"#,
                r#"{"e":"markPriceUpdate","E":1700000005000,"s":"BTCUSDT","p":"64010.20","i":"64010.00"}"#,
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                websocket
                    .send(Message::Text(payload.to_string()))
                    .await
                    .unwrap();
                websocket.close(None).await.unwrap();
            }
        });

        let rest = Arc::new(BinanceRestClient::new(
            "http://127.0.0.1:1",
            "api-key",
            "secret-key",
        ));
        let client = BinanceWsClient::with_reconnect_delay(
            rest,
            format!("ws://{}", address),
            Duration::from_millis(10),
        );

        let mut receiver = client
            .subscribe_prices(&Instrument::new(Venue::Binance, "BTCUSDT"))
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

        assert_eq!(first.mark_price, 64000.10);
        assert_eq!(second.mark_price, 64010.20);
    }
}
