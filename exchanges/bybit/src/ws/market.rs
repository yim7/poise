use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::{ExecutionQuote, ExecutionQuoteTick, MarkPriceTick, MarketDataTick};
use poise_engine::track::{Instrument, Venue};

use super::{backoff_delay, connect_websocket, models::PublicTickerMessage};

pub(super) async fn run_market_stream(
    url: String,
    symbol: String,
    sender: mpsc::Sender<MarketDataTick>,
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
                    let mut ticker_state = TickerState::new(&symbol);
                    while let Some(message) = websocket.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                match ticker_state.parse_linear_ticker_message(&text) {
                                    Ok(ticks) => {
                                        for tick in ticks {
                                            if sender.send(tick).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to parse market data message: {error}"
                                        );
                                    }
                                }
                            }
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

#[derive(Debug)]
struct TickerState {
    expected_symbol: String,
    last_quote: Option<ExecutionQuote>,
}

impl TickerState {
    fn new(expected_symbol: impl Into<String>) -> Self {
        Self {
            expected_symbol: expected_symbol.into(),
            last_quote: None,
        }
    }

    fn parse_linear_ticker_message(&mut self, payload: &str) -> Result<Vec<MarketDataTick>> {
        let value: serde_json::Value = serde_json::from_str(payload)?;
        let Some(topic) = value.get("topic").and_then(|topic| topic.as_str()) else {
            return Ok(vec![]);
        };
        if !topic.starts_with("tickers.") {
            return Ok(vec![]);
        }
        let message: PublicTickerMessage = serde_json::from_value(value)?;
        let Some(symbol) = message.topic.strip_prefix("tickers.") else {
            return Ok(vec![]);
        };
        if symbol != self.expected_symbol {
            return Err(anyhow!(
                "unexpected ticker topic: expected tickers.{}, got {}",
                self.expected_symbol,
                message.topic
            ));
        }

        let mark_price = message.data.mark_price;
        let execution_quote =
            self.merge_execution_quote(message.data.bid1_price, message.data.ask1_price);
        let timestamp = Utc
            .timestamp_millis_opt(message.ts)
            .single()
            .context("invalid ticker timestamp")?;

        let mut ticks = vec![];
        let instrument = Instrument::new(Venue::Bybit, &self.expected_symbol);
        if let Some(mark_price) = mark_price {
            ticks.push(MarketDataTick::MarkPrice(MarkPriceTick {
                instrument: instrument.clone(),
                mark_price,
                timestamp,
            }));
        }
        if let Some(execution_quote) = execution_quote {
            ticks.push(MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
                instrument,
                execution_quote,
                timestamp,
            }));
        }

        Ok(ticks)
    }

    fn merge_execution_quote(
        &mut self,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
    ) -> Option<ExecutionQuote> {
        let merged = match (best_bid, best_ask, self.last_quote.as_ref()) {
            (Some(best_bid), Some(best_ask), _) => Some(ExecutionQuote { best_bid, best_ask }),
            (Some(best_bid), None, Some(previous)) => Some(ExecutionQuote {
                best_bid,
                best_ask: previous.best_ask,
            }),
            (None, Some(best_ask), Some(previous)) => Some(ExecutionQuote {
                best_bid: previous.best_bid,
                best_ask,
            }),
            _ => None,
        };

        if let Some(quote) = merged.as_ref() {
            self.last_quote = Some(*quote);
        }

        merged
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_engine::ports::ExecutionQuote;
    use poise_engine::track::{Instrument, Venue};

    use super::*;
    use crate::ws::BybitWsClient;

    fn btc_ticker_state() -> TickerState {
        TickerState::new("BTCUSDT")
    }

    #[test]
    fn parses_bybit_ticker_mark_and_top_of_book_into_market_ticks() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000000000,
            "data": {
                "symbol": "BTCUSDT",
                "markPrice": "64000.10",
                "bid1Price": "63999.50",
                "ask1Price": "64000.50"
            }
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert_eq!(
            ticks,
            vec![
                MarketDataTick::MarkPrice(MarkPriceTick {
                    instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                    mark_price: 64000.10,
                    timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                }),
                MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
                    instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                    execution_quote: ExecutionQuote {
                        best_bid: 63999.50,
                        best_ask: 64000.50,
                    },
                    timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                })
            ]
        );
    }

    #[test]
    fn ticker_state_ignores_subscription_ack_messages() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "success": true,
            "op": "subscribe",
            "conn_id": "test"
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert!(ticks.is_empty());
    }

    #[test]
    fn ticker_state_ignores_delta_without_mark_or_quote() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "delta",
            "ts": 1700000005000,
            "data": {
                "symbol": "BTCUSDT",
                "lastPrice": "64010.20"
            }
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert!(ticks.is_empty());
    }

    #[test]
    fn ticker_state_ignores_delta_without_symbol_mark_or_quote() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "delta",
            "ts": 1700000005000,
            "data": {
                "lastPrice": "64010.20"
            }
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert!(ticks.is_empty());
    }

    #[test]
    fn emits_bybit_mark_price_tick_when_bid_or_ask_is_missing_without_cached_opposite_side() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000005000,
            "data": {
                "markPrice": "64010.20",
                "bid1Price": "64009.80"
            }
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert_eq!(
            ticks,
            vec![MarketDataTick::MarkPrice(MarkPriceTick {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                mark_price: 64010.20,
                timestamp: Utc.timestamp_millis_opt(1_700_000_005_000).unwrap(),
            })]
        );
    }

    #[test]
    fn ticker_state_merges_partial_quote_update_with_cached_opposite_side() {
        let mut state = btc_ticker_state();
        let snapshot = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000000000,
            "data": {
                "symbol": "BTCUSDT",
                "markPrice": "64000.10",
                "bid1Price": "63999.50",
                "ask1Price": "64000.50"
            }
        }"#;
        let delta = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000005000,
            "data": {
                "markPrice": "64010.20",
                "bid1Price": "64009.80"
            }
        }"#;

        let _ = state.parse_linear_ticker_message(snapshot).unwrap();
        let ticks = state.parse_linear_ticker_message(delta).unwrap();

        assert_eq!(
            ticks,
            vec![
                MarketDataTick::MarkPrice(MarkPriceTick {
                    instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                    mark_price: 64010.20,
                    timestamp: Utc.timestamp_millis_opt(1_700_000_005_000).unwrap(),
                }),
                MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
                    instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                    execution_quote: ExecutionQuote {
                        best_bid: 64009.80,
                        best_ask: 64000.50,
                    },
                    timestamp: Utc.timestamp_millis_opt(1_700_000_005_000).unwrap(),
                })
            ]
        );
    }

    #[test]
    fn ticker_state_does_not_emit_cached_quote_for_mark_only_update() {
        let mut state = btc_ticker_state();
        let snapshot = r#"{
            "topic": "tickers.BTCUSDT",
            "ts": 1700000000000,
            "data": {
                "symbol": "BTCUSDT",
                "markPrice": "64000.10",
                "bid1Price": "63999.50",
                "ask1Price": "64000.50"
            }
        }"#;
        let mark_only = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "delta",
            "ts": 1700000005000,
            "data": {
                "markPrice": "64010.20"
            }
        }"#;

        let _ = state.parse_linear_ticker_message(snapshot).unwrap();
        let ticks = state.parse_linear_ticker_message(mark_only).unwrap();

        assert_eq!(
            ticks,
            vec![MarketDataTick::MarkPrice(MarkPriceTick {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                mark_price: 64010.20,
                timestamp: Utc.timestamp_millis_opt(1_700_000_005_000).unwrap(),
            })]
        );
    }

    #[test]
    fn ticker_state_parses_delta_mark_price_with_symbol_from_topic() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "delta",
            "ts": 1700000005000,
            "data": {
                "markPrice": "64010.20"
            }
        }"#;

        let ticks = state.parse_linear_ticker_message(payload).unwrap();

        assert_eq!(
            ticks,
            vec![MarketDataTick::MarkPrice(MarkPriceTick {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                mark_price: 64010.20,
                timestamp: Utc.timestamp_millis_opt(1_700_000_005_000).unwrap(),
            })]
        );
    }

    #[test]
    fn ticker_state_does_not_emit_when_delta_has_no_mark_or_quote() {
        let mut state = btc_ticker_state();
        let snapshot = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "snapshot",
            "ts": 1700000000000,
            "data": {
                "symbol": "BTCUSDT",
                "markPrice": "64000.10"
            }
        }"#;
        let delta = r#"{
            "topic": "tickers.BTCUSDT",
            "type": "delta",
            "ts": 1700000005000,
            "data": {
                "lastPrice": "64010.20"
            }
        }"#;

        let _ = state.parse_linear_ticker_message(snapshot).unwrap();
        let ticks = state.parse_linear_ticker_message(delta).unwrap();

        assert!(ticks.is_empty());
    }

    #[test]
    fn ticker_state_rejects_unexpected_topic_symbol() {
        let mut state = btc_ticker_state();
        let payload = r#"{
            "topic": "tickers.ETHUSDT",
            "type": "snapshot",
            "ts": 1700000000000,
            "data": {
                "markPrice": "3200.10"
            }
        }"#;

        let error = state
            .parse_linear_ticker_message(payload)
            .unwrap_err()
            .to_string();

        assert!(error.contains("unexpected ticker topic"));
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
        match first {
            MarketDataTick::MarkPrice(tick) => assert_eq!(tick.mark_price, 64000.10),
            MarketDataTick::ExecutionQuote(_) => panic!("expected mark price tick"),
        }
        match second {
            MarketDataTick::MarkPrice(tick) => assert_eq!(tick.mark_price, 64010.20),
            MarketDataTick::ExecutionQuote(_) => panic!("expected mark price tick"),
        }
    }
}
