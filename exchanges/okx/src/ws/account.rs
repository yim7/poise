use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::{UserDataEvent, UserDataPayload};

use crate::Credentials;
use crate::mapper::{open_order_from_snapshot, position_from_snapshot};
use crate::rest::auth::sign_okx_payload;
use crate::rest::models::{PendingOrderSnapshot, PositionSnapshot};
use crate::ws::{backoff_delay, connect_websocket, models::UserMessage};

pub(super) async fn run_user_stream(
    url: String,
    credentials: Credentials,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    sender: mpsc::Sender<UserDataEvent>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_websocket(&url).await {
            Ok((mut websocket, _)) => {
                attempt = 0;
                if let Err(error) =
                    authenticate_and_subscribe(&mut websocket, &credentials, &timestamp_provider)
                        .await
                {
                    tracing::warn!("failed to authenticate OKX private stream: {error}");
                } else {
                    while let Some(message) = websocket.next().await {
                        match message {
                            Ok(Message::Text(text)) => match parse_user_data_message(&text) {
                                Ok(events) => {
                                    for event in events {
                                        if sender.send(event).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!("failed to parse OKX user message: {error}");
                                }
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(error) => {
                                super::log_websocket_error("OKX private", &error);
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect OKX private websocket: {error}");
            }
        }

        if sender.is_closed() {
            return;
        }

        sleep(backoff_delay(reconnect_delay, attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

async fn authenticate_and_subscribe(
    websocket: &mut super::WebSocket,
    credentials: &Credentials,
    timestamp_provider: &Arc<dyn Fn() -> i64 + Send + Sync>,
) -> Result<()> {
    websocket
        .send(Message::Text(build_login_payload(
            credentials.api_key(),
            credentials.api_secret(),
            credentials.passphrase(),
            timestamp_provider(),
        )))
        .await
        .context("failed to send OKX login payload")?;
    websocket
        .send(Message::Text(
            serde_json::json!({
                "op": "subscribe",
                "args": [
                    {"channel": "orders", "instType": "SWAP"},
                    {"channel": "positions", "instType": "SWAP"}
                ]
            })
            .to_string(),
        ))
        .await
        .context("failed to send OKX private subscribe payload")?;
    Ok(())
}

pub(crate) fn build_login_payload(
    api_key: &str,
    api_secret: &str,
    passphrase: &str,
    timestamp: i64,
) -> String {
    let timestamp = timestamp.to_string();
    let sign = sign_okx_payload(&timestamp, "GET", "/users/self/verify", "", api_secret);
    serde_json::json!({
        "op": "login",
        "args": [{
            "apiKey": api_key,
            "passphrase": passphrase,
            "timestamp": timestamp,
            "sign": sign
        }]
    })
    .to_string()
}

pub(crate) fn parse_user_data_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    if value.get("event").is_some() {
        return Ok(Vec::new());
    }
    let message: UserMessage = serde_json::from_value(value)?;
    let _inst_type = message.arg.inst_type.as_deref();

    match message.arg.channel.as_str() {
        "orders" => parse_orders(message.data),
        "positions" => parse_positions(message.data),
        _ => Ok(Vec::new()),
    }
}

fn parse_orders(data: Vec<serde_json::Value>) -> Result<Vec<UserDataEvent>> {
    let mut events = Vec::new();
    for value in data {
        let event_time = millis_to_utc(required_str(&value, "uTime")?)?;
        let order_snapshot: PendingOrderSnapshot = serde_json::from_value(value.clone())?;
        let order = open_order_from_snapshot(order_snapshot)?;
        events.push(UserDataEvent {
            event_time,
            payload: UserDataPayload::OrderUpdate(order.clone()),
        });

        let fill_size = optional_decimal(&value, "fillSz")?.unwrap_or(0.0);
        let trade_id = optional_str(&value, "tradeId");
        if fill_size > f64::EPSILON || trade_id.is_some() {
            let fill_price = optional_decimal(&value, "fillPx")?.unwrap_or(order.price);
            let realized_pnl = optional_decimal(&value, "fillPnl")?
                .or(optional_decimal(&value, "pnl")?)
                .unwrap_or(0.0);
            let raw_fee = optional_decimal(&value, "fillFee")?
                .or(optional_decimal(&value, "fee")?)
                .unwrap_or(0.0);
            let trading_fee = -raw_fee;
            let trade_id = trade_id.map(ToString::to_string);
            let source_key = trade_id.as_ref().map(|trade_id| {
                format!(
                    "okx:orders:{}:{}",
                    order.instrument.symbol.to_lowercase(),
                    trade_id
                )
            });
            events.push(UserDataEvent {
                event_time,
                payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                    order.instrument.clone(),
                    event_time,
                    "okx:orders".to_string(),
                    source_key,
                    Some(order.order_id.clone()),
                    trade_id,
                    order.side,
                    fill_price,
                    fill_size,
                    realized_pnl,
                    trading_fee,
                )),
            });
        }
    }
    Ok(events)
}

fn parse_positions(data: Vec<serde_json::Value>) -> Result<Vec<UserDataEvent>> {
    let mut events = Vec::new();
    for value in data {
        let event_time = millis_to_utc(required_str(&value, "uTime")?)?;
        let position: PositionSnapshot = serde_json::from_value(value)?;
        events.push(UserDataEvent {
            event_time,
            payload: UserDataPayload::PositionUpdate(position_from_snapshot(position)?),
        });
    }
    Ok(events)
}

fn millis_to_utc(value: &str) -> Result<chrono::DateTime<Utc>> {
    let timestamp_ms = value
        .parse::<i64>()
        .with_context(|| format!("invalid OKX timestamp: {value}"))?;
    Utc.timestamp_millis_opt(timestamp_ms)
        .single()
        .context("invalid OKX timestamp millis")
}

fn required_str<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("missing OKX field `{field}`"))
}

fn optional_str<'a>(value: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn optional_decimal(value: &serde_json::Value, field: &str) -> Result<Option<f64>> {
    let Some(value) = optional_str(value, field) else {
        return Ok(None);
    };
    value
        .parse::<f64>()
        .map(Some)
        .with_context(|| format!("invalid OKX decimal `{field}`: {value}"))
}
