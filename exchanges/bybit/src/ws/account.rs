use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_application::exchange::{ExchangeOrder, Position, UserDataEvent, UserDataPayload};
use poise_application::exchange::{TrackPnlQuantity, TrackPnlRecord};
use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionBasis, PositionQty};
use poise_core::track::{Instrument, Venue};

use super::{
    backoff_delay, connect_websocket,
    models::{
        ExecutionTopicMessage, ExecutionUpdate, OrderTopicMessage, OrderUpdate,
        PositionTopicMessage, PositionUpdate,
    },
};
use crate::instrument::BybitInstrumentRegistry;
use crate::mapper::{
    BybitActiveOrder, build_bybit_open_order, build_bybit_position, should_track_bybit_order,
};

const PRIVATE_EXECUTION_TOPIC: &str = "execution.linear";
const PRIVATE_ORDER_TOPIC: &str = "order.linear";
const PRIVATE_POSITION_TOPIC: &str = "position.linear";

pub(super) async fn run_user_stream(
    url: String,
    api_key: String,
    api_secret: String,
    timestamp_provider: Arc<dyn Fn() -> i64 + Send + Sync>,
    instrument_registry: Arc<BybitInstrumentRegistry>,
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
                            Ok(Message::Text(text)) => {
                                match parse_user_data_message(&text, &instrument_registry) {
                                    Ok(events) => {
                                        for event in events {
                                            if sender.send(event).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to parse user data message: {error}"
                                        );
                                    }
                                }
                            }
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
                "args": [PRIVATE_EXECUTION_TOPIC, PRIVATE_ORDER_TOPIC, PRIVATE_POSITION_TOPIC]
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

pub(super) fn parse_user_data_message(
    payload: &str,
    instrument_registry: &BybitInstrumentRegistry,
) -> Result<Vec<UserDataEvent>> {
    let value: serde_json::Value = serde_json::from_str(payload)?;
    let Some(topic) = value.get("topic").and_then(|topic| topic.as_str()) else {
        return Ok(Vec::new());
    };

    match topic {
        PRIVATE_EXECUTION_TOPIC => parse_execution_message(payload, instrument_registry),
        PRIVATE_ORDER_TOPIC => parse_order_message(payload),
        PRIVATE_POSITION_TOPIC => parse_position_message(payload),
        _ => Ok(Vec::new()),
    }
}

fn parse_execution_message(
    payload: &str,
    instrument_registry: &BybitInstrumentRegistry,
) -> Result<Vec<UserDataEvent>> {
    let message: ExecutionTopicMessage = serde_json::from_str(payload)?;
    if message.topic != PRIVATE_EXECUTION_TOPIC {
        return Ok(Vec::new());
    }
    let event_time = Utc
        .timestamp_millis_opt(message.creation_time)
        .single()
        .context("invalid execution event timestamp")?;

    let mut events = Vec::with_capacity(message.data.len());
    for execution in message.data {
        events.push(parse_execution_update(
            event_time,
            execution,
            instrument_registry,
        )?);
    }

    Ok(events)
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
        if !should_track_bybit_order(order.order_status, order.stop_order_type.as_deref()) {
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
        filled_qty: update.cum_exec_qty.unwrap_or(0.0),
        order_status: update.order_status,
        position_idx: update.position_idx,
    })
}

fn parse_position_update(update: PositionUpdate) -> Result<Position> {
    build_bybit_position(
        update.symbol,
        update.side,
        update.size,
        update.entry_price,
        update.unrealised_pnl,
        update.position_idx,
    )
}

fn parse_execution_update(
    event_time: chrono::DateTime<Utc>,
    update: ExecutionUpdate,
    instrument_registry: &BybitInstrumentRegistry,
) -> Result<UserDataEvent> {
    let symbol = update.symbol;
    let exec_id = update.exec_id;
    let instrument = Instrument::new(Venue::Bybit, symbol.clone());
    let instrument_spec = instrument_registry.get(&symbol).with_context(|| {
        format!("missing Bybit instrument metadata for execution symbol `{symbol}`")
    })?;
    let occurred_at = Utc
        .timestamp_millis_opt(update.exec_time)
        .single()
        .context("invalid execution trade timestamp")?;
    let settlement_asset = instrument_spec.settlement_asset().as_str();
    let fee_asset = normalized_fee_currency(update.fee_currency.as_deref())
        .context("missing feeCurrency for Bybit execution")?;

    Ok(UserDataEvent {
        event_time,
        payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
            instrument,
            occurred_at,
            "bybit:execution".into(),
            Some(format!(
                "bybit:execution:{}:{}",
                symbol.to_lowercase(),
                exec_id
            )),
            Some(update.order_id),
            Some(exec_id),
            update.side,
            update.exec_price,
            pnl_quantity(
                PositionQty::new(update.exec_qty).map_err(anyhow::Error::msg)?,
                &instrument_spec,
            )?,
            cash_amount(update.exec_pnl, settlement_asset)?,
            Some(cash_amount(update.exec_fee, fee_asset)?),
        )),
    })
}

fn pnl_quantity(qty: PositionQty, instrument_spec: &InstrumentSpec) -> Result<TrackPnlQuantity> {
    match instrument_spec.position_basis() {
        PositionBasis::BaseAsset => Ok(TrackPnlQuantity::base_asset(
            qty,
            instrument_spec.base_asset().clone(),
        )),
        PositionBasis::Contracts { contract_value } => {
            TrackPnlQuantity::contracts(qty, contract_value.clone()).map_err(anyhow::Error::msg)
        }
    }
}

fn cash_amount(value: f64, asset: &str) -> Result<Amount> {
    let asset = Asset::try_new(asset).map_err(anyhow::Error::msg)?;
    Amount::new(value, asset).map_err(anyhow::Error::msg)
}

fn normalized_fee_currency(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt, StreamExt};
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::*;
    use crate::ws::BybitWsClient;
    use poise_application::exchange::TrackPnlQuantity;
    use poise_core::instrument::PositionQty;
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;

    fn test_registry() -> BybitInstrumentRegistry {
        let registry = BybitInstrumentRegistry::default();
        registry.upsert(
            InstrumentSpec::try_linear(
                Instrument::new(Venue::Bybit, "BTCUSDT"),
                Asset::try_new("BTC").unwrap(),
                Asset::try_new("USDT").unwrap(),
                Asset::try_new("USDT").unwrap(),
                Default::default(),
                0.1,
                0.001,
                0.001,
                Amount::new(5.0, Asset::try_new("USDT").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        registry
    }

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

        let events = parse_user_data_message(payload, &test_registry()).unwrap();
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
                assert_eq!(order.qty.value(), 0.010);
                assert_eq!(order.status, poise_application::exchange::OrderStatus::New);
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

        let error = parse_user_data_message(payload, &test_registry())
            .unwrap_err()
            .to_string();

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

        let events = parse_user_data_message(payload, &test_registry()).unwrap();
        assert_eq!(events.len(), 1);

        let event = &events[0];
        match &event.payload {
            UserDataPayload::PositionUpdate(position) => {
                assert_eq!(
                    position.instrument,
                    Instrument::new(Venue::Bybit, "BTCUSDT")
                );
                assert_eq!(position.qty.value(), 0.010);
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

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert!(events.is_empty());
    }

    #[test]
    fn parses_execution_update_without_discarding_non_settlement_fee_asset() {
        let payload = r#"{
            "topic": "execution.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "execId": "exec-1",
                "orderId": "order-1",
                "side": "Sell",
                "execPrice": "64000.50",
                "execQty": "0.015",
                "execTime": "1699999999123",
                "execPnl": "12.34",
                "execFee": "3.21",
                "feeCurrency": "MNT"
            }]
        }"#;

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            events,
            vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                    Instrument::new(Venue::Bybit, "BTCUSDT"),
                    Utc.timestamp_millis_opt(1_699_999_999_123).unwrap(),
                    "bybit:execution".into(),
                    Some("bybit:execution:btcusdt:exec-1".into()),
                    Some("order-1".into()),
                    Some("exec-1".into()),
                    Side::Sell,
                    64_000.50,
                    TrackPnlQuantity::base_asset(
                        PositionQty::new(0.015).unwrap(),
                        Asset::try_new("BTC").unwrap(),
                    ),
                    cash_amount(12.34, "USDT").unwrap(),
                    Some(cash_amount(3.21, "MNT").unwrap()),
                )),
            }]
        );
    }

    #[test]
    fn rejects_execution_update_when_instrument_metadata_is_missing() {
        let payload = r#"{
            "topic": "execution.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "execId": "exec-1",
                "orderId": "order-1",
                "side": "Sell",
                "execPrice": "64000.50",
                "execQty": "0.015",
                "execTime": "1699999999123",
                "execPnl": "12.34",
                "execFee": "3.21",
                "feeCurrency": "USDT"
            }]
        }"#;

        let error = parse_user_data_message(payload, &BybitInstrumentRegistry::default())
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing Bybit instrument metadata"));
        assert!(error.contains("BTCUSDT"));
    }

    #[test]
    fn rejects_execution_update_with_empty_fee_currency() {
        let payload = r#"{
            "topic": "execution.linear",
            "creationTime": 1700000000000,
            "data": [{
                "symbol": "BTCUSDT",
                "execId": "exec-2",
                "orderId": "order-2",
                "side": "Buy",
                "execPrice": "63000.25",
                "execQty": "0.020",
                "execTime": "1699999999456",
                "execPnl": "0.50",
                "execFee": "1.25",
                "feeCurrency": ""
            }]
        }"#;

        let error = parse_user_data_message(payload, &test_registry())
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing feeCurrency"));
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
                                    r#"{"topic":"execution.linear","creationTime":1700000000000,"data":[{"symbol":"BTCUSDT","execId":"exec-bridge-1","orderId":"order-bridge-1","side":"Sell","execPrice":"64000.50","execQty":"0.015","execTime":"1699999999123","execPnl":"12.34","execFee":"3.21","feeCurrency":"USDT"}]}"#.to_string(),
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
        client
            .instrument_registry()
            .upsert(test_registry().get("BTCUSDT").unwrap());

        let mut receiver = client.subscribe_user_data().await.unwrap();
        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_time.timestamp_millis(), 1_700_000_000_000);
        assert_eq!(
            event,
            UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                    Instrument::new(Venue::Bybit, "BTCUSDT"),
                    Utc.timestamp_millis_opt(1_699_999_999_123).unwrap(),
                    "bybit:execution".into(),
                    Some("bybit:execution:btcusdt:exec-bridge-1".into()),
                    Some("order-bridge-1".into()),
                    Some("exec-bridge-1".into()),
                    Side::Sell,
                    64_000.50,
                    TrackPnlQuantity::base_asset(
                        PositionQty::new(0.015).unwrap(),
                        Asset::try_new("BTC").unwrap(),
                    ),
                    cash_amount(12.34, "USDT").unwrap(),
                    Some(cash_amount(3.21, "USDT").unwrap()),
                )),
            }
        );

        let messages = messages.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"op\":\"auth\""));
        assert!(messages[1].contains("\"op\":\"subscribe\""));
        assert!(messages[1].contains("execution.linear"));
        assert!(messages[1].contains("order.linear"));
        assert!(messages[1].contains("position.linear"));
    }
}
