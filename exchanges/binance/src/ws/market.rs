use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::{ExecutionQuote, PriceTick};
use poise_engine::track::{Instrument, Venue};

use super::{
    backoff_delay, connect_websocket, log_websocket_error,
    models::{BookTickerMessage, MarkPriceMessage, MarketEvent, MarketStreamEnvelope},
    parse_decimal,
};

pub(super) async fn run_market_stream(
    url: String,
    symbol: String,
    sender: mpsc::Sender<PriceTick>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;
    let mut market_state = BinanceMarketState::new(symbol);

    loop {
        match connect_websocket(&url).await {
            Ok((mut websocket, _)) => {
                attempt = 0;

                while let Some(message) = websocket.next().await {
                    match message {
                        Ok(Message::Text(text)) => match market_state.parse_message(&text) {
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

#[derive(Debug)]
pub(super) struct BinanceMarketState {
    expected_symbol: String,
    last_mark_price: Option<f64>,
    last_quote: Option<ExecutionQuote>,
}

impl BinanceMarketState {
    pub(super) fn new(expected_symbol: impl Into<String>) -> Self {
        Self {
            expected_symbol: expected_symbol.into(),
            last_mark_price: None,
            last_quote: None,
        }
    }

    pub(super) fn parse_message(&mut self, payload: &str) -> Result<Option<PriceTick>> {
        let envelope: MarketStreamEnvelope = serde_json::from_str(payload)?;
        let event = match envelope {
            MarketStreamEnvelope::Combined { data } => data,
            MarketStreamEnvelope::Plain(data) => data,
        };

        match event {
            MarketEvent::MarkPrice(message) => self.parse_mark_price(message),
            MarketEvent::BookTicker(message) => self.parse_book_ticker(message),
        }
    }

    fn parse_mark_price(&mut self, message: MarkPriceMessage) -> Result<Option<PriceTick>> {
        self.ensure_symbol(&message.symbol)?;
        let mark_price = parse_decimal("p", &message.mark_price)?;
        self.last_mark_price = Some(mark_price);

        Ok(Some(PriceTick {
            instrument: Instrument::new(Venue::Binance, &self.expected_symbol),
            mark_price,
            execution_quote: self.last_quote.clone(),
            timestamp: parse_timestamp(message.event_time)?,
        }))
    }

    fn parse_book_ticker(&mut self, message: BookTickerMessage) -> Result<Option<PriceTick>> {
        self.ensure_symbol(&message.symbol)?;
        let quote = match (message.best_bid.as_deref(), message.best_ask.as_deref()) {
            (Some(best_bid), Some(best_ask)) => {
                let quote = ExecutionQuote {
                    best_bid: parse_decimal("b", best_bid)?,
                    best_ask: parse_decimal("a", best_ask)?,
                };
                self.last_quote = Some(quote.clone());
                quote
            }
            _ => return Ok(None),
        };
        let Some(mark_price) = self.last_mark_price else {
            return Ok(None);
        };

        Ok(Some(PriceTick {
            instrument: Instrument::new(Venue::Binance, &self.expected_symbol),
            mark_price,
            execution_quote: Some(quote),
            timestamp: parse_timestamp(message.event_time)?,
        }))
    }

    fn ensure_symbol(&self, actual_symbol: &str) -> Result<()> {
        if actual_symbol == self.expected_symbol {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "unexpected market symbol: expected {}, got {}",
                self.expected_symbol,
                actual_symbol
            ))
        }
    }
}

fn parse_timestamp(timestamp_millis: i64) -> Result<chrono::DateTime<Utc>> {
    Utc.timestamp_millis_opt(timestamp_millis)
        .single()
        .context("invalid event timestamp")
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
    use poise_engine::ports::ExecutionQuote;
    use crate::rest::BinanceRestClient;
    use crate::ws::BinanceWsClient;

    #[test]
    fn parses_binance_mark_and_book_into_price_tick() {
        let mark_payload = r#"{
            "e": "markPriceUpdate",
            "E": 1700000000000,
            "s": "BTCUSDT",
            "p": "64000.10",
            "i": "63999.90"
        }"#;
        let book_payload = r#"{
            "e": "bookTicker",
            "E": 1700000000000,
            "s": "BTCUSDT",
            "b": "63999.50",
            "B": "2.000",
            "a": "64000.50",
            "A": "3.000"
        }"#;
        let mut state = BinanceMarketState::new("BTCUSDT");

        let first = state.parse_message(mark_payload).unwrap().unwrap();
        let second = state.parse_message(book_payload).unwrap().unwrap();

        assert_eq!(
            first,
            PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                mark_price: 64000.10,
                execution_quote: None,
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            }
        );
        assert_eq!(
            second,
            PriceTick {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                mark_price: 64000.10,
                execution_quote: Some(ExecutionQuote {
                    best_bid: 63999.50,
                    best_ask: 64000.50,
                }),
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            }
        );
    }

    #[test]
    fn ignores_binance_book_update_until_bid_and_ask_are_both_present() {
        let payload = r#"{
            "e": "bookTicker",
            "E": 1700000000000,
            "s": "BTCUSDT",
            "b": "63999.50",
            "B": "2.000"
        }"#;
        let mut state = BinanceMarketState::new("BTCUSDT");

        let tick = state.parse_message(payload).unwrap();

        assert!(tick.is_none());
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
