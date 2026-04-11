use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_engine::ports::{ExchangeOrder, Position, UserDataEvent, UserDataPayload};

use super::{
    backoff_delay, connect_websocket,
    models::{OrderTopicMessage, OrderUpdate, PositionTopicMessage, PositionUpdate},
};
use crate::mapper::{
    BybitActiveOrder, build_bybit_open_order, build_bybit_position, should_track_bybit_order,
};

const PRIVATE_ORDER_TOPIC: &str = "order.linear";
const PRIVATE_POSITION_TOPIC: &str = "position.linear";

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
                "args": [PRIVATE_ORDER_TOPIC, PRIVATE_POSITION_TOPIC]
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
        PRIVATE_ORDER_TOPIC => parse_order_message(payload),
        PRIVATE_POSITION_TOPIC => parse_position_message(payload),
        _ => Ok(Vec::new()),
    }
}

fn parse_order_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let message: OrderTopicMessage = serde_json::from_str(payload)?;
    if message.topic != PRIVATE_ORDER_TOPIC {
        return Ok(Vec::new());
    }
    let event_time = Utc
        .timestamp_millis_opt(message.creation_time)
        .single()
        .context("invalid order event timestamp")?;

    let mut events = Vec::with_capacity(message.data.len());
    for order in message.data {
        if !should_track_bybit_order(
            order.order_status.as_str(),
            order.stop_order_type.as_deref(),
        ) {
            continue;
        }
        events.push(UserDataEvent {
            event_time,
            payload: UserDataPayload::OrderUpdate(parse_order_update(order)?),
        });
    }

    Ok(events)
}

fn parse_position_message(payload: &str) -> Result<Vec<UserDataEvent>> {
    let message: PositionTopicMessage = serde_json::from_str(payload)?;
    if message.topic != PRIVATE_POSITION_TOPIC {
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
    build_bybit_open_order(BybitActiveOrder {
        symbol: update.symbol,
        order_id: update.order_id,
        client_order_id: update.order_link_id,
        side: update.side,
        price: update.price,
        qty: update.qty,
        order_status: update.order_status,
        position_idx: update.position_idx,
    })
}

fn parse_position_update(update: PositionUpdate) -> Result<Position> {
    build_bybit_position(
        update.symbol,
        update.side.as_deref(),
        &update.size,
        &update.entry_price,
        &update.unrealised_pnl,
        update.position_idx,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use poise_core::types::Side;
    use poise_engine::track::{Instrument, Venue};

    use super::*;
    use crate::ws::BybitWsClient;

    #[test]
    fn parses_order_update_into_user_data_event() {
        let payload = r#"{
            "topic": "order.linear",
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
            "topic": "position.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "side": "Sell",
                "size": "0.010",
                "entryPrice": "64000.10",
                "unrealisedPnl": "-1.25",
                "positionIdx": 1
            }]
        }"#;

        let error = parse_user_data_message(payload).unwrap_err().to_string();

        assert!(error.contains("one-way"));
    }

    #[test]
    fn parses_position_update_with_entry_price_field() {
        let payload = r#"{
            "topic": "position.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "side": "Buy",
                "size": "0.010",
                "entryPrice": "64000.10",
                "unrealisedPnl": "1.25",
                "positionIdx": 0
            }]
        }"#;

        let events = parse_user_data_message(payload).unwrap();
        assert_eq!(events.len(), 1);

        let event = &events[0];
        match &event.payload {
            UserDataPayload::PositionUpdate(position) => {
                assert_eq!(
                    position.instrument,
                    Instrument::new(Venue::Bybit, "BTCUSDT")
                );
                assert_eq!(position.qty, 0.010);
                assert_eq!(position.avg_price, 64000.10);
                assert_eq!(position.unrealized_pnl, 1.25);
            }
            other => panic!("expected position update, got {other:?}"),
        }
    }

    #[test]
    fn ignores_conditional_order_update() {
        let payload = r#"{
            "topic": "order.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "orderId": "123",
                "orderLinkId": "client-1",
                "side": "Buy",
                "price": "64000.10",
                "qty": "0.010",
                "orderStatus": "Untriggered",
                "stopOrderType": "Stop",
                "positionIdx": 0
            }]
        }"#;

        let events = parse_user_data_message(payload).unwrap();

        assert!(events.is_empty());
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
                                    r#"{"topic":"order.linear","creationTime":1700000000000,"data":[{"symbol":"BTCUSDT","orderId":"123","orderLinkId":"client-1","side":"Buy","price":"64000.10","qty":"0.010","orderStatus":"New","positionIdx":0}]}"#.to_string(),
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
        assert!(messages[1].contains("order.linear"));
        assert!(messages[1].contains("position.linear"));
    }
}
