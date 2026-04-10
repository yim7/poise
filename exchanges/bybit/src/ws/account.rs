use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::{ExchangeOrder, Position, UserDataEvent, UserDataPayload};
use poise_engine::track::{Instrument, Venue};

use super::{
    backoff_delay, connect_websocket,
    models::{OrderTopicMessage, OrderUpdate, PositionTopicMessage, PositionUpdate},
};

pub(super) async fn run_user_stream(
    url: String,
    api_key: String,
    api_secret: String,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    sender: mpsc::Sender<UserDataEvent>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_websocket(&url).await {
            Ok((mut websocket, _)) => {
                attempt = 0;
                if let Err(error) = authenticate_and_subscribe(
                    &mut websocket,
                    &api_key,
                    &api_secret,
                    Arc::clone(&timestamp_provider),
                )
                .await
                {
                    tracing::warn!("failed to authenticate bybit private stream: {error}");
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
                                    tracing::warn!("failed to parse user data message: {error}");
                                }
                            },
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(error) => {
                                super::log_websocket_error("private", &error);
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect private websocket: {error}");
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
    api_key: &str,
    api_secret: &str,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
) -> Result<()> {
    let expires_ms = timestamp_provider() + 5_000;
    let auth_payload = build_auth_payload(api_key, api_secret, expires_ms);
    websocket
        .send(Message::Text(auth_payload))
        .await
        .context("failed to send bybit auth payload")?;
    websocket
        .send(Message::Text(
            serde_json::json!({
                "op": "subscribe",
                "args": ["order", "position"]
            })
            .to_string(),
        ))
        .await
        .context("failed to send bybit subscribe payload")?;
    Ok(())
}

fn build_auth_payload(api_key: &str, api_secret: &str, expires_ms: i64) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let signing_payload = format!("GET/realtime{expires_ms}");
    let mut mac = Hmac::<Sha256>::new_from_slice(api_secret.as_bytes())
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(signing_payload.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    serde_json::json!({
        "op": "auth",
        "args": [api_key, expires_ms, signature]
    })
    .to_string()
}

pub(super) fn parse_user_data_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    let Some(topic) = value.get("topic").and_then(|topic| topic.as_str()) else {
        return Ok(Vec::new());
    };

    match topic {
        "order" => parse_order_message(payload),
        "position" => parse_position_message(payload),
        _ => Ok(Vec::new()),
    }
}

fn parse_order_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let message: OrderTopicMessage = serde_json::from_str(payload)?;
    if message.topic != "order" {
        return Ok(Vec::new());
    }
    let event_time = Utc
        .timestamp_millis_opt(message.creation_time)
        .single()
        .context("invalid order event timestamp")?;

    let mut events = Vec::with_capacity(message.data.len());
    for order in message.data {
        events.push(UserDataEvent {
            event_time,
            payload: UserDataPayload::OrderUpdate(parse_order_update(order)?),
        });
    }

    Ok(events)
}

fn parse_position_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let message: PositionTopicMessage = serde_json::from_str(payload)?;
    if message.topic != "position" {
        return Ok(Vec::new());
    }
    let event_time = Utc
        .timestamp_millis_opt(message.creation_time)
        .single()
        .context("invalid position event timestamp")?;

    let mut events = Vec::with_capacity(message.data.len());
    for position in message.data {
        events.push(UserDataEvent {
            event_time,
            payload: UserDataPayload::PositionUpdate(parse_position_update(position)?),
        });
    }

    Ok(events)
}

fn parse_order_update(update: OrderUpdate) -> Result<ExchangeOrder> {
    require_one_way(update.position_idx)?;
    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Bybit, update.symbol),
        order_id: update.order_id,
        client_order_id: update.order_link_id.unwrap_or_default(),
        side: parse_side(&update.side)?,
        price: parse_decimal("price", &update.price)?,
        qty: parse_decimal("qty", &update.qty)?,
        realized_pnl: 0.0,
        status: parse_order_status(&update.order_status)?,
    })
}

fn parse_position_update(update: PositionUpdate) -> Result<Position> {
    require_one_way(update.position_idx)?;
    let side_multiplier = match update.side.as_deref() {
        Some("Buy") | Some("buy") | None => 1.0,
        Some("Sell") | Some("sell") => -1.0,
        Some(other) => return Err(anyhow!("unsupported Bybit position side: {other}")),
    };

    Ok(Position {
        instrument: Instrument::new(Venue::Bybit, update.symbol),
        qty: parse_decimal("size", &update.size)? * side_multiplier,
        avg_price: parse_decimal("avgPrice", &update.avg_price)?,
        unrealized_pnl: parse_decimal("unrealisedPnl", &update.unrealised_pnl)?,
    })
}

fn require_one_way(position_idx: i64) -> Result<()> {
    if position_idx != 0 {
        return Err(anyhow!(
            "Bybit private stream only supports one-way positions; positionIdx must be 0"
        ));
    }
    Ok(())
}

fn parse_order_status(value: &str) -> Result<poise_engine::ports::OrderStatus> {
    match value {
        "New" | "NEW" => Ok(poise_engine::ports::OrderStatus::New),
        "PartiallyFilled" | "PARTIALLY_FILLED" => {
            Ok(poise_engine::ports::OrderStatus::PartiallyFilled)
        }
        "Filled" | "FILLED" => Ok(poise_engine::ports::OrderStatus::Filled),
        "Cancelled" | "CANCELED" => Ok(poise_engine::ports::OrderStatus::Canceled),
        "Rejected" | "REJECTED" => Ok(poise_engine::ports::OrderStatus::Rejected),
        "Expired" | "EXPIRED" => Ok(poise_engine::ports::OrderStatus::Expired),
        other => Err(anyhow!("unsupported Bybit order status: {other}")),
    }
}

fn parse_side(value: &str) -> Result<poise_core::types::Side> {
    match value {
        "Buy" | "BUY" | "buy" => Ok(poise_core::types::Side::Buy),
        "Sell" | "SELL" | "sell" => Ok(poise_core::types::Side::Sell),
        other => Err(anyhow!("unsupported Bybit side: {other}")),
    }
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

    use poise_core::types::Side;

    use super::*;
    use crate::ws::BybitWsClient;

    #[test]
    fn parses_order_update_into_user_data_event() {
        let payload = r#"{
            "topic": "order",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "orderId": "123",
                "orderLinkId": "client-1",
                "side": "Buy",
                "price": "64000.10",
                "qty": "0.010",
                "orderStatus": "New",
                "positionIdx": 0
            }]
        }"#;

        let events = parse_user_data_message(payload).unwrap();
        assert_eq!(events.len(), 1);

        let event = &events[0];
        assert_eq!(event.event_time.timestamp_millis(), 1_700_000_000_000);
        match &event.payload {
            UserDataPayload::OrderUpdate(order) => {
                assert_eq!(order.instrument, Instrument::new(Venue::Bybit, "BTCUSDT"));
                assert_eq!(order.order_id, "123");
                assert_eq!(order.client_order_id, "client-1");
                assert_eq!(order.side, Side::Buy);
                assert_eq!(order.price, 64000.10);
                assert_eq!(order.qty, 0.010);
                assert_eq!(order.status, poise_engine::ports::OrderStatus::New);
            }
            other => panic!("expected order update, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_one_way_position_update() {
        let payload = r#"{
            "topic": "position",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "side": "Sell",
                "size": "0.010",
                "avgPrice": "64000.10",
                "unrealisedPnl": "-1.25",
                "positionIdx": 1
            }]
        }"#;

        let error = parse_user_data_message(payload).unwrap_err().to_string();

        assert!(error.contains("positionIdx must be 0"));
    }

    #[tokio::test]
    async fn auth_and_subscribe_bridge_private_events() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let server_messages = Arc::clone(&messages);

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();

            while let Some(message) = websocket.next().await {
                match message.unwrap() {
                    Message::Text(text) => {
                        server_messages.lock().unwrap().push(text.clone());
                        if server_messages.lock().unwrap().len() == 2 {
                            websocket
                                .send(Message::Text(r#"{"success":true,"op":"auth"}"#.to_string()))
                                .await
                                .unwrap();
                            websocket
                                .send(Message::Text(
                                    r#"{"success":true,"op":"subscribe"}"#.to_string(),
                                ))
                                .await
                                .unwrap();
                            websocket
                                .send(Message::Text(
                                    r#"{"topic":"order","creationTime":1700000000000,"data":[{"symbol":"BTCUSDT","orderId":"123","orderLinkId":"client-1","side":"Buy","price":"64000.10","qty":"0.010","orderStatus":"New","positionIdx":0}]}"#.to_string(),
                                ))
                                .await
                                .unwrap();
                            websocket.close(None).await.unwrap();
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
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

        let mut receiver = client.subscribe_user_data().await.unwrap();
        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_time.timestamp_millis(), 1_700_000_000_000);

        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"op\":\"auth\""));
        assert!(messages[1].contains("\"op\":\"subscribe\""));
    }
}
