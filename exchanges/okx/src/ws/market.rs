use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_core::track::{Instrument, Venue};
use poise_engine::ports::{ExecutionQuote, ExecutionQuoteTick, MarkPriceTick, MarketDataTick};

use super::{backoff_delay, connect_websocket, models::MarketMessage};

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
                    tracing::warn!("failed to subscribe OKX market stream: {error}");
                } else {
                    while let Some(message) = websocket.next().await {
                        match message {
                            Ok(Message::Text(text)) => match parse_market_message(&symbol, &text) {
                                Ok(ticks) => {
                                    for tick in ticks {
                                        if sender.send(tick).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!("failed to parse OKX market message: {error}");
                                }
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(error) => {
                                super::log_websocket_error("OKX market", &error);
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect OKX market websocket: {error}");
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
        "args": [
            {"channel": "tickers", "instId": symbol},
            {"channel": "mark-price", "instId": symbol}
        ]
    });
    websocket
        .send(Message::Text(payload.to_string()))
        .await
        .with_context(|| format!("failed to subscribe OKX market stream `{symbol}`"))?;
    Ok(())
}

pub(crate) fn parse_market_message(symbol: &str, payload: &str) -> Result<Vec<MarketDataTick>> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    if value.get("event").is_some() {
        return Ok(Vec::new());
    }

    let message: MarketMessage = serde_json::from_value(value)?;
    let channel = message.arg.channel.as_str();
    if channel != "tickers" && channel != "mark-price" {
        return Ok(Vec::new());
    }
    if message.arg.inst_id.as_deref() != Some(symbol) {
        return Err(anyhow!(
            "unexpected OKX market instrument: expected {symbol}, got {:?}",
            message.arg.inst_id
        ));
    }

    let mut ticks = Vec::new();
    for data in message.data {
        if data.inst_id != symbol {
            return Err(anyhow!(
                "unexpected OKX market data instrument: expected {symbol}, got {}",
                data.inst_id
            ));
        }
        let timestamp_ms = parse_i64("ts", &data.ts)?;
        let timestamp = Utc
            .timestamp_millis_opt(timestamp_ms)
            .single()
            .context("invalid OKX market timestamp")?;
        let instrument = Instrument::new(Venue::Okx, symbol);

        match channel {
            "tickers" => {
                let best_bid = parse_optional_f64("bidPx", data.bid_px.as_deref())?
                    .context("missing OKX bidPx")?;
                let best_ask = parse_optional_f64("askPx", data.ask_px.as_deref())?
                    .context("missing OKX askPx")?;
                ticks.push(MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
                    instrument,
                    execution_quote: ExecutionQuote { best_bid, best_ask },
                    timestamp,
                }));
            }
            "mark-price" => {
                let mark_price = parse_optional_f64("markPx", data.mark_px.as_deref())?
                    .context("missing OKX markPx")?;
                ticks.push(MarketDataTick::MarkPrice(MarkPriceTick {
                    instrument,
                    mark_price,
                    timestamp,
                }));
            }
            _ => {}
        }
    }

    Ok(ticks)
}

fn parse_i64(field: &str, value: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .with_context(|| format!("invalid integer for {field}: {value}"))
}

fn parse_optional_f64(field: &str, value: Option<&str>) -> Result<Option<f64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value
        .parse::<f64>()
        .map(Some)
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}
