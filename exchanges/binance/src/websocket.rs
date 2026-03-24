use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::{
    net::TcpStream,
    sync::mpsc,
    time::{Duration, Instant, interval_at, sleep},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use grid_engine::ports::{PriceTick, UserDataEvent, UserDataPayload};

use crate::rest::BinanceRestClient;

type UserWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct BinanceWsClient {
    #[allow(dead_code)]
    rest: Arc<BinanceRestClient>,
    #[allow(dead_code)]
    ws_base_url: String,
    #[allow(dead_code)]
    reconnect_delay: Duration,
}

impl BinanceWsClient {
    pub fn new(rest: Arc<BinanceRestClient>, ws_base_url: impl Into<String>) -> Self {
        Self {
            rest,
            ws_base_url: ws_base_url.into().trim_end_matches('/').to_string(),
            reconnect_delay: Duration::from_millis(250),
        }
    }

    #[cfg(test)]
    fn with_reconnect_delay(
        rest: Arc<BinanceRestClient>,
        ws_base_url: impl Into<String>,
        reconnect_delay: Duration,
    ) -> Self {
        Self {
            rest,
            ws_base_url: ws_base_url.into().trim_end_matches('/').to_string(),
            reconnect_delay,
        }
    }

    pub async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = format!(
            "{}/ws/{}@markPrice",
            self.ws_base_url,
            symbol.to_lowercase()
        );
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            run_market_stream(url, sender, reconnect_delay).await;
        });

        Ok(receiver)
    }

    pub async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (sender, receiver) = mpsc::channel(128);
        let ws_base_url = self.ws_base_url.clone();
        let rest = Arc::clone(&self.rest);
        let reconnect_delay = self.reconnect_delay;
        let initial_listen_key = rest.start_user_stream().await?;
        let initial_websocket = connect_user_stream(&ws_base_url, &initial_listen_key).await?;

        tokio::spawn(async move {
            run_user_stream(
                ws_base_url,
                rest,
                initial_listen_key,
                Some(initial_websocket),
                sender,
                reconnect_delay,
            )
            .await;
        });

        Ok(receiver)
    }
}

async fn run_market_stream(
    url: String,
    sender: mpsc::Sender<PriceTick>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_async(&url).await {
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
                            tracing::warn!("market data websocket error: {error}");
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

async fn run_user_stream(
    ws_base_url: String,
    rest: Arc<BinanceRestClient>,
    initial_listen_key: String,
    mut initial_websocket: Option<UserWebSocket>,
    sender: mpsc::Sender<UserDataEvent>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;
    let mut listen_key = initial_listen_key;

    loop {
        let connection = match initial_websocket.take() {
            Some(websocket) => Ok(websocket),
            None => connect_user_stream(&ws_base_url, &listen_key).await,
        };

        match connection {
            Ok(mut websocket) => {
                attempt = 0;
                let mut keepalive = interval_at(
                    Instant::now() + Duration::from_secs(30 * 60),
                    Duration::from_secs(30 * 60),
                );

                loop {
                    tokio::select! {
                        message = websocket.next() => {
                            match message {
                                Some(Ok(Message::Text(text))) => {
                                    match parse_user_data_message(&text) {
                                        Ok(UserStreamMessage::Events(events)) => {
                                            for event in events {
                                                if sender.send(event).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                        Ok(UserStreamMessage::ListenKeyExpired) => break,
                                        Err(error) => {
                                            tracing::warn!("failed to parse user data message: {error}");
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                Some(Ok(_)) => {}
                                Some(Err(error)) => {
                                    tracing::warn!("user data websocket error: {error}");
                                    break;
                                }
                            }
                        }
                        _ = keepalive.tick() => {
                            if let Err(error) = rest.keepalive_user_stream(&listen_key).await {
                                tracing::warn!("failed to keepalive listen key: {error}");
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("{error}");
            }
        }

        if sender.is_closed() {
            return;
        }

        sleep(backoff_delay(reconnect_delay, attempt)).await;
        attempt = attempt.saturating_add(1);

        match rest.start_user_stream().await {
            Ok(next_listen_key) => {
                listen_key = next_listen_key;
            }
            Err(error) => {
                tracing::warn!("failed to create listen key: {error}");
            }
        }
    }
}

async fn connect_user_stream(ws_base_url: &str, listen_key: &str) -> Result<UserWebSocket> {
    let url = format!("{ws_base_url}/ws/{listen_key}");
    let (websocket, _) = connect_async(&url)
        .await
        .with_context(|| format!("failed to connect user data websocket `{url}`"))?;
    Ok(websocket)
}

fn parse_mark_price_message(payload: &str) -> Result<Option<PriceTick>> {
    let message: MarkPriceMessage = serde_json::from_str(payload)?;
    let mark_price = parse_decimal("p", &message.mark_price)?;
    let timestamp = Utc
        .timestamp_millis_opt(message.event_time)
        .single()
        .context("invalid event timestamp")?;

    Ok(Some(PriceTick {
        symbol: message.symbol,
        last_price: mark_price,
        mark_price,
        timestamp,
    }))
}

fn parse_user_data_message(payload: &str) -> Result<UserStreamMessage> {
    let envelope: UserEventEnvelope = serde_json::from_str(payload)?;
    let event_time = Utc
        .timestamp_millis_opt(envelope.event_time)
        .single()
        .context("invalid user event timestamp")?;

    match envelope.event_type.as_str() {
        "ORDER_TRADE_UPDATE" => {
            let order = envelope
                .order
                .context("missing order payload for ORDER_TRADE_UPDATE")?;

            Ok(UserStreamMessage::Events(vec![UserDataEvent {
                event_time,
                payload: UserDataPayload::OrderUpdate(grid_engine::ports::OpenOrder {
                    symbol: order.symbol,
                    order_id: order.order_id.to_string(),
                    client_order_id: order.client_order_id,
                    side: parse_side(&order.side)?,
                    price: parse_decimal("o.p", &order.price)?,
                    qty: parse_decimal("o.q", &order.quantity)?,
                    realized_pnl: parse_decimal("o.rp", &order.realized_pnl)?,
                    status: order.status,
                }),
            }]))
        }
        "ACCOUNT_UPDATE" => {
            let account = envelope
                .account
                .context("missing account payload for ACCOUNT_UPDATE")?;

            let events = account
                .positions
                .into_iter()
                .map(|position| {
                    Ok(UserDataEvent {
                        event_time,
                        payload: UserDataPayload::PositionUpdate(grid_engine::ports::Position {
                            symbol: position.symbol,
                            qty: parse_decimal("a.P.pa", &position.position_amt)?,
                            avg_price: parse_decimal("a.P.ep", &position.entry_price)?,
                            unrealized_pnl: parse_decimal("a.P.up", &position.unrealized_profit)?,
                        }),
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            Ok(UserStreamMessage::Events(events))
        }
        "listenKeyExpired" => Ok(UserStreamMessage::ListenKeyExpired),
        _ => Ok(UserStreamMessage::Events(Vec::new())),
    }
}

fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(4)).unwrap_or(16);
    base.saturating_mul(multiplier)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn parse_side(value: &str) -> Result<grid_core::types::Side> {
    match value {
        "BUY" => Ok(grid_core::types::Side::Buy),
        "SELL" => Ok(grid_core::types::Side::Sell),
        other => Err(anyhow!("unsupported side: {other}")),
    }
}

#[derive(Debug, Deserialize)]
struct MarkPriceMessage {
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "p")]
    mark_price: String,
}

#[derive(Debug, Deserialize)]
struct UserEventEnvelope {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: i64,
    #[serde(rename = "o")]
    order: Option<OrderTradeUpdate>,
    #[serde(rename = "a")]
    account: Option<AccountUpdate>,
}

#[derive(Debug, Deserialize)]
struct OrderTradeUpdate {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "i")]
    order_id: u64,
    #[serde(rename = "c")]
    client_order_id: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "rp")]
    realized_pnl: String,
    #[serde(rename = "X")]
    status: String,
}

#[derive(Debug, Deserialize)]
struct AccountUpdate {
    #[serde(rename = "P")]
    positions: Vec<AccountPositionUpdate>,
}

#[derive(Debug, Deserialize)]
struct AccountPositionUpdate {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "pa")]
    position_amt: String,
    #[serde(rename = "ep")]
    entry_price: String,
    #[serde(rename = "up")]
    unrealized_profit: String,
}

#[derive(Debug, PartialEq)]
enum UserStreamMessage {
    Events(Vec<UserDataEvent>),
    ListenKeyExpired,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, collections::VecDeque, sync::Arc};

    use chrono::{TimeZone, Utc};
    use futures_util::SinkExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, Notify},
        time::timeout,
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use grid_core::types::Side;
    use grid_engine::ports::{OpenOrder, Position, UserDataPayload};

    use super::*;

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
                symbol: "BTCUSDT".to_string(),
                last_price: 64000.10,
                mark_price: 64000.10,
                timestamp: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            }
        );
    }

    #[test]
    fn parses_order_trade_update_message() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "p": "65000.5",
                "q": "0.020",
                "rp": "12.34",
                "X": "FILLED"
            }
        }"#;

        let events = parse_user_data_message(payload).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::OrderUpdate(OpenOrder {
                    symbol: "BTCUSDT".to_string(),
                    order_id: "12345".to_string(),
                    client_order_id: "grid-order-004".to_string(),
                    side: Side::Sell,
                    price: 65000.5,
                    qty: 0.02,
                    realized_pnl: 12.34,
                    status: "FILLED".to_string(),
                }),
            }])
        );
    }

    #[test]
    fn parses_account_update_message() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "a": {
                "P": [{
                    "s": "BTCUSDT",
                    "pa": "0.015",
                    "ep": "64200.0",
                    "up": "12.3"
                }]
            }
        }"#;

        let events = parse_user_data_message(payload).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::PositionUpdate(Position {
                    symbol: "BTCUSDT".to_string(),
                    qty: 0.015,
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                }),
            }])
        );
    }

    #[test]
    fn parses_listen_key_expired_message() {
        let payload = r#"{
            "e": "listenKeyExpired",
            "E": 1700000000000,
            "listenKey": "listen-key-1"
        }"#;

        let event = parse_user_data_message(payload).unwrap();

        assert_eq!(event, UserStreamMessage::ListenKeyExpired);
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

        let mut receiver = client.subscribe_prices("BTCUSDT").await.unwrap();
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

    #[tokio::test]
    async fn reconnects_user_data_stream_after_listen_key_expired() {
        let rest_server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#"{"listenKey":"listen-key-1"}"#),
            MockResponse::json(200, r#"{"listenKey":"listen-key-2"}"#),
        ])
        .await;
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (first_stream, _) = ws_listener.accept().await.unwrap();
            let mut first_ws = accept_async(first_stream).await.unwrap();
            first_ws
                .send(Message::Text(
                    r#"{"e":"listenKeyExpired","E":1700000000000,"listenKey":"listen-key-1"}"#
                        .to_string(),
                ))
                .await
                .unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = first_ws.close(None).await;
            });

            let (second_stream, _) = ws_listener.accept().await.unwrap();
            let mut second_ws = accept_async(second_stream).await.unwrap();
            second_ws
                .send(
                    Message::Text(
                        r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"a":{"P":[{"s":"BTCUSDT","pa":"0.015","ep":"64200.0","up":"12.3"}]}}"#
                            .to_string()
                    ),
                )
                .await
                .unwrap();
            second_ws.close(None).await.unwrap();
        });

        let rest = Arc::new(BinanceRestClient::new(
            rest_server.base_url(),
            "api-key",
            "secret-key",
        ));
        let client = BinanceWsClient::with_reconnect_delay(
            rest,
            format!("ws://{}", ws_address),
            Duration::from_millis(10),
        );

        let mut receiver = client.subscribe_user_data().await.unwrap();
        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let requests = rest_server.requests().await;

        assert_eq!(
            event,
            UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::PositionUpdate(Position {
                    symbol: "BTCUSDT".to_string(),
                    qty: 0.015,
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                }),
            }
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == "/fapi/v1/listenKey" && request.method == "POST")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn subscribe_user_data_waits_for_initial_connection_before_returning() {
        let rest_server = MockHttpServer::spawn(vec![MockResponse::json(
            200,
            r#"{"listenKey":"listen-key-1"}"#,
        )])
        .await;
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();
        let accept_gate = Arc::new(Notify::new());
        let server_gate = Arc::clone(&accept_gate);

        tokio::spawn(async move {
            server_gate.notified().await;
            let (stream, _) = ws_listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            websocket
                .send(
                    Message::Text(
                        r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"a":{"P":[{"s":"BTCUSDT","pa":"0.015","ep":"64200.0","up":"12.3"}]}}"#
                            .to_string(),
                    ),
                )
                .await
                .unwrap();
            websocket.close(None).await.unwrap();
        });

        let rest = Arc::new(BinanceRestClient::new(
            rest_server.base_url(),
            "api-key",
            "secret-key",
        ));
        let client = BinanceWsClient::with_reconnect_delay(
            rest,
            format!("ws://{}", ws_address),
            Duration::from_millis(10),
        );

        let mut subscription = tokio::spawn(async move { client.subscribe_user_data().await });

        assert!(
            timeout(Duration::from_millis(50), &mut subscription)
                .await
                .is_err()
        );

        accept_gate.notify_one();

        let mut receiver = timeout(Duration::from_secs(1), &mut subscription)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            event,
            UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::PositionUpdate(Position {
                    symbol: "BTCUSDT".to_string(),
                    qty: 0.015,
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                }),
            }
        );
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
    }

    impl MockResponse {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        path: String,
        headers: HashMap<String, String>,
    }

    struct MockHttpServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl MockHttpServer {
        async fn spawn(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let queued_responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let stored_requests = Arc::clone(&requests);

            tokio::spawn(async move {
                loop {
                    let response = {
                        let mut queue = queued_responses.lock().await;
                        queue.pop_front()
                    };

                    let Some(response) = response else {
                        break;
                    };

                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = Vec::new();
                    let mut chunk = [0_u8; 1024];

                    loop {
                        let read = stream.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let request_text = String::from_utf8(buffer).unwrap();
                    let mut lines = request_text.split("\r\n");
                    let request_line = lines.next().unwrap();
                    let mut request_line_parts = request_line.split_whitespace();
                    let method = request_line_parts.next().unwrap().to_string();
                    let path = request_line_parts.next().unwrap().to_string();
                    let mut headers = HashMap::new();

                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            headers
                                .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                        }
                    }

                    stored_requests.lock().await.push(RecordedRequest {
                        method,
                        path,
                        headers,
                    });

                    let reply = format!(
                        "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response.status,
                        reason_phrase(response.status),
                        response.body.len(),
                        response.body
                    );

                    stream.write_all(reply.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });

            Self {
                base_url: format!("http://{}", address),
                requests,
            }
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        async fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().await.clone()
        }
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            _ => "Unknown",
        }
    }
}
