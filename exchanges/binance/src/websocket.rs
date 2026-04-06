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
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async, connect_async_tls_with_config,
    tungstenite::{Error as WebSocketError, Message, error::ProtocolError},
};

use poise_engine::ledger::{
    ExecutionLedgerUpdate, LedgerAdjustmentEvent, LedgerDelta, LedgerGapRecord, TrackLedgerEvent,
};
use poise_engine::observation::OrderObservation;
use poise_engine::ports::{PriceTick, TrackLedgerUpdate, UserDataEvent, UserDataPayload};
use poise_engine::track::{Instrument, Venue};

use crate::rest::BinanceRestClient;
use crate::types::parse_order_status;

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

    pub async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<PriceTick>> {
        let (sender, receiver) = mpsc::channel(128);
        let url = format!(
            "{}/ws/{}@markPrice",
            self.ws_base_url,
            instrument.symbol.to_lowercase()
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
                let mut diagnostics = UserStreamDiagnostics::new(Instant::now());
                let mut keepalive = interval_at(
                    Instant::now() + Duration::from_secs(30 * 60),
                    Duration::from_secs(30 * 60),
                );

                loop {
                    tokio::select! {
                        message = websocket.next() => {
                            match message {
                                Some(Ok(Message::Text(text))) => {
                                    diagnostics.record_message(Instant::now());
                                    match parse_user_data_message(&text) {
                                        Ok(UserStreamMessage::Events(events)) => {
                                            for event in events {
                                                let send_started = Instant::now();
                                                if sender.send(event).await.is_err() {
                                                    return;
                                                }
                                                diagnostics.record_send_wait(send_started.elapsed());
                                            }
                                        }
                                        Ok(UserStreamMessage::ListenKeyExpired) => {
                                            log_user_stream_disconnect(
                                                "listen_key_expired",
                                                diagnostics.disconnect_snapshot(Instant::now()),
                                            );
                                            break;
                                        }
                                        Err(error) => {
                                            tracing::warn!("failed to parse user data message: {error}");
                                        }
                                    }
                                }
                                Some(Ok(Message::Close(_))) => {
                                    log_user_stream_disconnect(
                                        "close_frame",
                                        diagnostics.disconnect_snapshot(Instant::now()),
                                    );
                                    break;
                                }
                                Some(Ok(_)) => {
                                    diagnostics.record_message(Instant::now());
                                }
                                None => {
                                    log_user_stream_disconnect(
                                        "stream_ended",
                                        diagnostics.disconnect_snapshot(Instant::now()),
                                    );
                                    break;
                                }
                                Some(Err(error)) => {
                                    log_user_stream_error(
                                        &error,
                                        diagnostics.disconnect_snapshot(Instant::now()),
                                    );
                                    break;
                                }
                            }
                        }
                        _ = keepalive.tick() => {
                            let started_at = Instant::now();
                            match rest.keepalive_user_stream(&listen_key).await {
                                Ok(()) => diagnostics.record_keepalive_result(
                                    started_at,
                                    Instant::now(),
                                    KeepaliveStatus::Ok,
                                ),
                                Err(error) => {
                                    diagnostics.record_keepalive_result(
                                        started_at,
                                        Instant::now(),
                                        KeepaliveStatus::Failed,
                                    );
                                    let snapshot = diagnostics.disconnect_snapshot(Instant::now());
                                    tracing::warn!(
                                        error = %error,
                                        connection_age = ?snapshot.connection_age,
                                        idle_for = ?snapshot.idle_for,
                                        last_keepalive_age = ?snapshot.last_keepalive_age,
                                        last_keepalive_latency = ?snapshot.last_keepalive_latency,
                                        last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
                                        last_send_wait = ?snapshot.last_send_wait,
                                        max_send_wait = ?snapshot.max_send_wait,
                                        "failed to keepalive listen key; reconnecting"
                                    );
                                    break;
                                }
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
    let (websocket, _) = connect_websocket(&url)
        .await
        .with_context(|| format!("failed to connect user data websocket `{url}`"))?;
    Ok(websocket)
}

async fn connect_websocket(
    url: &str,
) -> Result<(
    WebSocketStream<MaybeTlsStream<TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    let connector = websocket_connector(url)?;
    let result = match connector {
        Some(connector) => connect_async_tls_with_config(url, None, false, Some(connector)).await,
        None => connect_async(url).await,
    };

    result.with_context(|| format!("failed to connect websocket `{url}`"))
}

fn websocket_connector(url: &str) -> Result<Option<Connector>> {
    if !url.starts_with("wss://") {
        return Ok(None);
    }

    let connector = native_tls::TlsConnector::builder()
        .build()
        .context("failed to build native TLS websocket connector")?;

    Ok(Some(Connector::NativeTls(connector)))
}

fn log_websocket_error(stream_name: &str, error: &WebSocketError) {
    if is_expected_disconnect(error) {
        tracing::info!("{stream_name} websocket disconnected: {error}; reconnecting");
    } else {
        tracing::warn!("{stream_name} websocket error: {error}");
    }
}

fn is_expected_disconnect(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::ConnectionClosed
            | WebSocketError::AlreadyClosed
            | WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepaliveStatus {
    Ok,
    Failed,
}

impl KeepaliveStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct KeepaliveObservation {
    finished_at: Instant,
    latency: Duration,
    status: KeepaliveStatus,
}

#[derive(Debug, Clone)]
struct UserStreamDiagnostics {
    connected_at: Instant,
    last_message_at: Instant,
    last_keepalive: Option<KeepaliveObservation>,
    last_send_wait: Option<Duration>,
    max_send_wait: Duration,
}

impl UserStreamDiagnostics {
    fn new(now: Instant) -> Self {
        Self {
            connected_at: now,
            last_message_at: now,
            last_keepalive: None,
            last_send_wait: None,
            max_send_wait: Duration::ZERO,
        }
    }

    fn record_message(&mut self, now: Instant) {
        self.last_message_at = now;
    }

    fn record_send_wait(&mut self, wait: Duration) {
        self.last_send_wait = Some(wait);
        if wait > self.max_send_wait {
            self.max_send_wait = wait;
        }
    }

    fn record_keepalive_result(
        &mut self,
        started_at: Instant,
        finished_at: Instant,
        status: KeepaliveStatus,
    ) {
        self.last_keepalive = Some(KeepaliveObservation {
            finished_at,
            latency: finished_at.saturating_duration_since(started_at),
            status,
        });
    }

    fn disconnect_snapshot(&self, now: Instant) -> UserStreamDisconnectSnapshot {
        UserStreamDisconnectSnapshot {
            connection_age: now.saturating_duration_since(self.connected_at),
            idle_for: now.saturating_duration_since(self.last_message_at),
            last_keepalive_age: self
                .last_keepalive
                .map(|observation| now.saturating_duration_since(observation.finished_at)),
            last_keepalive_latency: self.last_keepalive.map(|observation| observation.latency),
            last_keepalive_status: self.last_keepalive.map(|observation| observation.status),
            last_send_wait: self.last_send_wait,
            max_send_wait: self.max_send_wait,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UserStreamDisconnectSnapshot {
    connection_age: Duration,
    idle_for: Duration,
    last_keepalive_age: Option<Duration>,
    last_keepalive_latency: Option<Duration>,
    last_keepalive_status: Option<KeepaliveStatus>,
    last_send_wait: Option<Duration>,
    max_send_wait: Duration,
}

fn log_user_stream_disconnect(reason: &str, snapshot: UserStreamDisconnectSnapshot) {
    tracing::info!(
        reason,
        connection_age = ?snapshot.connection_age,
        idle_for = ?snapshot.idle_for,
        last_keepalive_age = ?snapshot.last_keepalive_age,
        last_keepalive_latency = ?snapshot.last_keepalive_latency,
        last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
        last_send_wait = ?snapshot.last_send_wait,
        max_send_wait = ?snapshot.max_send_wait,
        "user data websocket disconnected; reconnecting"
    );
}

fn log_user_stream_error(error: &WebSocketError, snapshot: UserStreamDisconnectSnapshot) {
    if is_expected_disconnect(error) {
        tracing::info!(
            error = %error,
            connection_age = ?snapshot.connection_age,
            idle_for = ?snapshot.idle_for,
            last_keepalive_age = ?snapshot.last_keepalive_age,
            last_keepalive_latency = ?snapshot.last_keepalive_latency,
            last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
            last_send_wait = ?snapshot.last_send_wait,
            max_send_wait = ?snapshot.max_send_wait,
            "user data websocket disconnected; reconnecting"
        );
    } else {
        tracing::warn!(
            error = %error,
            connection_age = ?snapshot.connection_age,
            idle_for = ?snapshot.idle_for,
            last_keepalive_age = ?snapshot.last_keepalive_age,
            last_keepalive_latency = ?snapshot.last_keepalive_latency,
            last_keepalive_status = snapshot.last_keepalive_status.map(KeepaliveStatus::as_str).unwrap_or("none"),
            last_send_wait = ?snapshot.last_send_wait,
            max_send_wait = ?snapshot.max_send_wait,
            "user data websocket error"
        );
    }
}

fn parse_mark_price_message(payload: &str) -> Result<Option<PriceTick>> {
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
            let realized_pnl = parse_decimal("o.rp", &order.realized_pnl)?;
            let price = parse_decimal("o.p", &order.price)?;
            let quantity = parse_decimal("o.q", &order.quantity)?;
            let instrument = Instrument::new(Venue::Binance, order.symbol.clone());
            let mut ledger_deltas = vec![LedgerDelta::GrossRealizedPnl(realized_pnl)];
            let mut ledger_gaps = Vec::new();
            if let Some(commission_amount) = order
                .commission_amount
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                let commission_amount = parse_decimal("o.n", commission_amount)?;
                if commission_amount.abs() > f64::EPSILON {
                    match (
                        order.commission_asset.as_deref(),
                        quote_asset_for_symbol(&order.symbol),
                    ) {
                        (Some(asset), Some(quote_asset)) if asset == quote_asset => {
                            ledger_deltas.push(LedgerDelta::TradingFee(commission_amount));
                        }
                        (Some(_), _) => ledger_gaps.push(LedgerGapRecord {
                            gap_key: format!(
                                "binance:order_trade_update:{}:{}:commission_asset",
                                order.symbol.to_lowercase(),
                                order.order_id
                            ),
                            reason: "unsupported_commission_asset".into(),
                            observed_at: event_time,
                            source: "binance:order_trade_update".into(),
                        }),
                        (None, _) => ledger_gaps.push(LedgerGapRecord {
                            gap_key: format!(
                                "binance:order_trade_update:{}:{}:missing_commission_asset",
                                order.symbol.to_lowercase(),
                                order.order_id
                            ),
                            reason: "missing_commission_asset".into(),
                            observed_at: event_time,
                            source: "binance:order_trade_update".into(),
                        }),
                    }
                }
            }

            Ok(UserStreamMessage::Events(vec![UserDataEvent {
                event_time,
                payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                    instrument,
                    event: TrackLedgerEvent::Execution(ExecutionLedgerUpdate {
                        order_update: OrderObservation {
                            order_id: order.order_id.to_string(),
                            client_order_id: order.client_order_id,
                            side: parse_side(&order.side)?,
                            price,
                            quantity,
                            realized_pnl,
                            status: parse_order_status(&order.status)?,
                        },
                        ledger_deltas,
                        ledger_gaps,
                    }),
                }),
            }]))
        }
        "ACCOUNT_UPDATE" => {
            let account = envelope
                .account
                .context("missing account payload for ACCOUNT_UPDATE")?;
            if account.reason.as_deref() == Some("FUNDING_FEE") {
                let Some(symbol) = account
                    .positions
                    .iter()
                    .map(|position| position.symbol.as_str())
                    .find(|symbol| !symbol.is_empty())
                else {
                    return Ok(UserStreamMessage::Events(Vec::new()));
                };
                let Some(balance) = account
                    .balances
                    .iter()
                    .find(|balance| balance.balance_change != "0" && balance.balance_change != "0.0")
                else {
                    return Ok(UserStreamMessage::Events(Vec::new()));
                };
                let balance_change = parse_decimal("a.B.bc", &balance.balance_change)?;
                let mut ledger_deltas = Vec::new();
                let mut ledger_gaps = Vec::new();
                match quote_asset_for_symbol(symbol) {
                    Some(quote_asset) if quote_asset == balance.asset => {
                        ledger_deltas.push(LedgerDelta::FundingFee(balance_change));
                    }
                    _ => ledger_gaps.push(LedgerGapRecord {
                        gap_key: format!(
                            "binance:funding_fee:{}:{}:asset",
                            symbol.to_lowercase(),
                            balance.asset.to_lowercase()
                        ),
                        reason: "unsupported_funding_asset".into(),
                        observed_at: event_time,
                        source: "binance:funding_fee".into(),
                    }),
                }

                return Ok(UserStreamMessage::Events(vec![UserDataEvent {
                    event_time,
                    payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                        instrument: Instrument::new(Venue::Binance, symbol.to_string()),
                        event: TrackLedgerEvent::Adjustment(LedgerAdjustmentEvent {
                            ledger_deltas,
                            ledger_gaps,
                            source: "binance:funding_fee".into(),
                        }),
                    }),
                }]));
            }

            let events = account
                .positions
                .into_iter()
                .map(|position| {
                    Ok(UserDataEvent {
                        event_time,
                        payload: UserDataPayload::PositionUpdate(poise_engine::ports::Position {
                            instrument: Instrument::new(Venue::Binance, position.symbol),
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

fn parse_side(value: &str) -> Result<poise_core::types::Side> {
    match value {
        "BUY" => Ok(poise_core::types::Side::Buy),
        "SELL" => Ok(poise_core::types::Side::Sell),
        other => Err(anyhow!("unsupported side: {other}")),
    }
}

fn quote_asset_for_symbol(symbol: &str) -> Option<&'static str> {
    ["USDT", "USDC", "FDUSD", "BUSD"]
        .into_iter()
        .find(|quote| symbol.ends_with(quote))
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
    #[serde(rename = "n")]
    commission_amount: Option<String>,
    #[serde(rename = "N")]
    commission_asset: Option<String>,
    #[serde(rename = "X")]
    status: String,
}

#[derive(Debug, Deserialize)]
struct AccountUpdate {
    #[serde(rename = "m")]
    reason: Option<String>,
    #[serde(rename = "B", default)]
    balances: Vec<AccountBalanceUpdate>,
    #[serde(rename = "P")]
    positions: Vec<AccountPositionUpdate>,
}

#[derive(Debug, Deserialize)]
struct AccountBalanceUpdate {
    #[serde(rename = "a")]
    asset: String,
    #[serde(rename = "bc")]
    balance_change: String,
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
    use tokio_tungstenite::{
        accept_async,
        tungstenite::{Error as WebSocketError, Message, error::ProtocolError},
    };

    use poise_core::types::Side;
    use poise_engine::ledger::{
        ExecutionLedgerUpdate, LedgerAdjustmentEvent, LedgerDelta, LedgerGapRecord,
        TrackLedgerEvent,
    };
    use poise_engine::observation::OrderObservation;
    use poise_engine::ports::{OrderStatus, Position, TrackLedgerUpdate, UserDataPayload};
    use poise_engine::track::{Instrument, Venue};

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
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                reference_price: 64000.10,
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
                payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    event: TrackLedgerEvent::Execution(ExecutionLedgerUpdate {
                        order_update: OrderObservation {
                            order_id: "12345".into(),
                            client_order_id: "grid-order-004".into(),
                            side: Side::Sell,
                            price: 65000.5,
                            quantity: 0.02,
                            realized_pnl: 12.34,
                            status: OrderStatus::Filled,
                        },
                        ledger_deltas: vec![LedgerDelta::GrossRealizedPnl(12.34)],
                        ledger_gaps: vec![],
                    }),
                }),
            }])
        );
    }

    #[test]
    fn parses_order_trade_update_into_track_ledger_execution_event() {
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
                "n": "3.2",
                "N": "USDT",
                "X": "FILLED"
            }
        }"#;

        let events = parse_user_data_message(payload).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    event: TrackLedgerEvent::Execution(ExecutionLedgerUpdate {
                        order_update: OrderObservation {
                            order_id: "12345".into(),
                            client_order_id: "grid-order-004".into(),
                            side: Side::Sell,
                            price: 65000.5,
                            quantity: 0.02,
                            realized_pnl: 12.34,
                            status: OrderStatus::Filled,
                        },
                        ledger_deltas: vec![
                            LedgerDelta::GrossRealizedPnl(12.34),
                            LedgerDelta::TradingFee(3.2),
                        ],
                        ledger_gaps: vec![],
                    }),
                }),
            }])
        );
    }

    #[test]
    fn parses_funding_fee_account_update_into_track_ledger_adjustment_event() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "a": {
                "m": "FUNDING_FEE",
                "B": [{
                    "a": "USDT",
                    "bc": "-1.5"
                }],
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
                payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    event: TrackLedgerEvent::Adjustment(LedgerAdjustmentEvent {
                        ledger_deltas: vec![LedgerDelta::FundingFee(-1.5)],
                        ledger_gaps: vec![],
                        source: "binance:funding_fee".into(),
                    }),
                }),
            }])
        );
    }

    #[test]
    fn parses_unsupported_commission_asset_into_execution_gap_record() {
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
                "n": "0.01",
                "N": "BNB",
                "X": "FILLED"
            }
        }"#;

        let events = parse_user_data_message(payload).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    event: TrackLedgerEvent::Execution(ExecutionLedgerUpdate {
                        order_update: OrderObservation {
                            order_id: "12345".into(),
                            client_order_id: "grid-order-004".into(),
                            side: Side::Sell,
                            price: 65000.5,
                            quantity: 0.02,
                            realized_pnl: 12.34,
                            status: OrderStatus::Filled,
                        },
                        ledger_deltas: vec![LedgerDelta::GrossRealizedPnl(12.34)],
                        ledger_gaps: vec![LedgerGapRecord {
                            gap_key: "binance:order_trade_update:btcusdt:12345:commission_asset".into(),
                            reason: "unsupported_commission_asset".into(),
                            observed_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                            source: "binance:order_trade_update".into(),
                        }],
                    }),
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
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
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
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
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
    async fn reconnects_user_data_stream_after_reset_without_close_handshake() {
        let rest_server = MockHttpServer::spawn(vec![
            MockResponse::json(200, r#"{"listenKey":"listen-key-1"}"#),
            MockResponse::json(200, r#"{"listenKey":"listen-key-2"}"#),
        ])
        .await;
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_address = ws_listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (first_stream, _) = ws_listener.accept().await.unwrap();
            let first_ws = accept_async(first_stream).await.unwrap();
            drop(first_ws);

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
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
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
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: 0.015,
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                }),
            }
        );
    }

    #[test]
    fn websocket_connector_uses_native_tls_for_secure_urls() {
        let connector = super::websocket_connector("wss://example.com/ws").unwrap();

        assert!(matches!(
            connector,
            Some(tokio_tungstenite::Connector::NativeTls(_))
        ));
    }

    #[test]
    fn websocket_connector_skips_tls_for_plain_urls() {
        let connector = super::websocket_connector("ws://127.0.0.1:18081/ws").unwrap();

        assert!(connector.is_none());
    }

    #[test]
    fn treats_reset_without_close_handshake_as_expected_disconnect() {
        let error = WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake);

        assert!(super::is_expected_disconnect(&error));
    }

    #[test]
    fn user_stream_diagnostics_snapshot_tracks_keepalive_and_send_wait() {
        let base = Instant::now();
        let mut diagnostics = super::UserStreamDiagnostics::new(base);

        diagnostics.record_message(base + Duration::from_secs(5));
        diagnostics.record_send_wait(Duration::from_millis(250));
        diagnostics.record_send_wait(Duration::from_millis(100));
        diagnostics.record_keepalive_result(
            base + Duration::from_secs(6),
            base + Duration::from_secs(8),
            super::KeepaliveStatus::Ok,
        );

        let snapshot = diagnostics.disconnect_snapshot(base + Duration::from_secs(20));

        assert_eq!(
            snapshot,
            super::UserStreamDisconnectSnapshot {
                connection_age: Duration::from_secs(20),
                idle_for: Duration::from_secs(15),
                last_keepalive_age: Some(Duration::from_secs(12)),
                last_keepalive_latency: Some(Duration::from_secs(2)),
                last_keepalive_status: Some(super::KeepaliveStatus::Ok),
                last_send_wait: Some(Duration::from_millis(100)),
                max_send_wait: Duration::from_millis(250),
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
