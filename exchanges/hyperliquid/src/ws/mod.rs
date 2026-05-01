use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use poise_core::track::{Instrument, Venue};
use poise_core::types::Side;
use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::{
    ExchangeOrder, ExecutionQuote, ExecutionQuoteTick, MarkPriceTick, MarketDataTick, OrderStatus,
    UserDataEvent, UserDataPayload,
};
use tokio::{
    sync::mpsc,
    time::{Duration, sleep},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_millis(250);

pub struct HyperliquidWsClient {
    ws_url: String,
    wallet_address: String,
    reconnect_delay: Duration,
}

impl HyperliquidWsClient {
    pub fn new(ws_url: impl Into<String>, wallet_address: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            wallet_address: wallet_address.into(),
            reconnect_delay: DEFAULT_RECONNECT_DELAY,
        }
    }

    #[cfg(test)]
    fn with_reconnect_delay(
        ws_url: impl Into<String>,
        wallet_address: impl Into<String>,
        reconnect_delay: Duration,
    ) -> Self {
        Self {
            ws_url: ws_url.into(),
            wallet_address: wallet_address.into(),
            reconnect_delay,
        }
    }

    pub async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>> {
        let symbol = instrument.symbol.clone();
        let (sender, receiver) = mpsc::channel(128);
        let ws_url = self.ws_url.clone();
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            run_market_stream(ws_url, symbol, sender, reconnect_delay).await;
        });
        Ok(receiver)
    }

    pub async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (sender, receiver) = mpsc::channel(128);
        let ws_url = self.ws_url.clone();
        let wallet_address = self.wallet_address.clone();
        let reconnect_delay = self.reconnect_delay;

        tokio::spawn(async move {
            run_user_stream(ws_url, wallet_address, sender, reconnect_delay).await;
        });
        Ok(receiver)
    }
}

async fn run_market_stream(
    ws_url: String,
    symbol: String,
    sender: mpsc::Sender<MarketDataTick>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                attempt = 0;
                if let Err(error) = subscribe_market(&mut ws, &symbol).await {
                    tracing::warn!("failed to subscribe Hyperliquid market stream: {error}");
                } else {
                    while let Some(message) = ws.next().await {
                        match message {
                            Ok(Message::Text(text)) => {
                                match parse_market_data_message(&text, &symbol) {
                                    Ok(Some(tick)) => {
                                        if sender.send(tick).await.is_err() {
                                            return;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        tracing::warn!(
                                            "failed to parse Hyperliquid market message: {error}"
                                        );
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => {
                                tracing::info!("Hyperliquid market websocket closed; reconnecting");
                                break;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(
                                    "Hyperliquid market websocket error: {error}; reconnecting"
                                );
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect Hyperliquid market websocket: {error}");
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
    ws_url: String,
    wallet_address: String,
    sender: mpsc::Sender<UserDataEvent>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;

    loop {
        match connect_async(&ws_url).await {
            Ok((mut ws, _)) => {
                attempt = 0;
                if let Err(error) = subscribe_user(&mut ws, &wallet_address).await {
                    tracing::warn!("failed to subscribe Hyperliquid user stream: {error}");
                } else {
                    while let Some(message) = ws.next().await {
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
                                    tracing::warn!(
                                        "failed to parse Hyperliquid user message: {error}"
                                    );
                                }
                            },
                            Ok(Message::Close(_)) => {
                                tracing::info!("Hyperliquid user websocket closed; reconnecting");
                                break;
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(
                                    "Hyperliquid user websocket error: {error}; reconnecting"
                                );
                                break;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("failed to connect Hyperliquid user websocket: {error}");
            }
        }

        if sender.is_closed() {
            return;
        }

        sleep(backoff_delay(reconnect_delay, attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

async fn subscribe_market<S>(ws: &mut S, symbol: &str) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    send_subscription(ws, serde_json::json!({ "type": "bbo", "coin": symbol })).await?;
    send_subscription(
        ws,
        serde_json::json!({ "type": "activeAssetCtx", "coin": symbol }),
    )
    .await?;
    Ok(())
}

async fn subscribe_user<S>(ws: &mut S, wallet_address: &str) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    send_subscription(
        ws,
        serde_json::json!({ "type": "orderUpdates", "user": wallet_address }),
    )
    .await?;
    send_subscription(
        ws,
        serde_json::json!({ "type": "userEvents", "user": wallet_address }),
    )
    .await?;
    Ok(())
}

async fn send_subscription<S>(ws: &mut S, subscription: serde_json::Value) -> Result<()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    ws.send(Message::Text(
        serde_json::json!({
            "method": "subscribe",
            "subscription": subscription,
        })
        .to_string(),
    ))
    .await
    .context("failed to send Hyperliquid websocket subscription")
}

fn backoff_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(4)).unwrap_or(16);
    base.saturating_mul(multiplier)
}

pub(crate) fn parse_market_data_message(
    message: &str,
    expected_symbol: &str,
) -> Result<Option<MarketDataTick>> {
    let value: serde_json::Value =
        serde_json::from_str(message).context("invalid Hyperliquid websocket JSON")?;
    match value["channel"].as_str() {
        Some("bbo") => parse_bbo(&value["data"], expected_symbol).map(Some),
        Some("activeAssetCtx") => parse_active_asset_ctx(&value["data"], expected_symbol).map(Some),
        _ => Ok(None),
    }
}

pub(crate) fn parse_user_data_message(message: &str) -> Result<Vec<UserDataEvent>> {
    let value: serde_json::Value =
        serde_json::from_str(message).context("invalid Hyperliquid websocket JSON")?;
    match value["channel"].as_str() {
        Some("orderUpdates") => parse_order_updates(&value["data"]),
        Some("userEvents") => parse_user_events(&value["data"]),
        _ => Ok(Vec::new()),
    }
}

fn parse_bbo(data: &serde_json::Value, expected_symbol: &str) -> Result<MarketDataTick> {
    let symbol = required_str(data, "coin")?;
    if symbol != expected_symbol {
        return Err(anyhow!(
            "Hyperliquid bbo symbol `{symbol}` does not match `{expected_symbol}`"
        ));
    }
    let bbo = data["bbo"]
        .as_array()
        .context("missing Hyperliquid bbo array")?;
    let bid = bbo
        .first()
        .and_then(|level| level.get("px"))
        .and_then(serde_json::Value::as_str)
        .context("missing Hyperliquid best bid")?;
    let ask = bbo
        .get(1)
        .and_then(|level| level.get("px"))
        .and_then(serde_json::Value::as_str)
        .context("missing Hyperliquid best ask")?;
    Ok(MarketDataTick::ExecutionQuote(ExecutionQuoteTick {
        instrument: Instrument::new(Venue::Hyperliquid, symbol),
        execution_quote: ExecutionQuote {
            best_bid: parse_decimal("bbo.bid.px", bid)?,
            best_ask: parse_decimal("bbo.ask.px", ask)?,
        },
        timestamp: millis_to_utc(required_i64(data, "time")?)?,
    }))
}

fn parse_active_asset_ctx(
    data: &serde_json::Value,
    expected_symbol: &str,
) -> Result<MarketDataTick> {
    let symbol = required_str(data, "coin")?;
    if symbol != expected_symbol {
        return Err(anyhow!(
            "Hyperliquid activeAssetCtx symbol `{symbol}` does not match `{expected_symbol}`"
        ));
    }
    Ok(MarketDataTick::MarkPrice(MarkPriceTick {
        instrument: Instrument::new(Venue::Hyperliquid, symbol),
        mark_price: parse_decimal("ctx.markPx", required_str(&data["ctx"], "markPx")?)?,
        timestamp: millis_to_utc(data["time"].as_i64().unwrap_or(0))?,
    }))
}

fn parse_order_updates(data: &serde_json::Value) -> Result<Vec<UserDataEvent>> {
    data.as_array()
        .context("Hyperliquid orderUpdates data must be an array")?
        .iter()
        .map(parse_order_update)
        .collect()
}

fn parse_order_update(value: &serde_json::Value) -> Result<UserDataEvent> {
    let order = &value["order"];
    let timestamp = required_i64(value, "statusTimestamp")?;
    let order_id = required_u64(order, "oid")?.to_string();
    Ok(UserDataEvent {
        event_time: millis_to_utc(timestamp)?,
        payload: UserDataPayload::OrderUpdate(ExchangeOrder {
            instrument: Instrument::new(Venue::Hyperliquid, required_str(order, "coin")?),
            order_id: order_id.clone(),
            client_order_id: order
                .get("cloid")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&order_id)
                .to_string(),
            side: parse_side(required_str(order, "side")?)?,
            price: parse_decimal("order.limitPx", required_str(order, "limitPx")?)?,
            qty: parse_decimal("order.origSz", required_str(order, "origSz")?)?,
            filled_qty: 0.0,
            status: parse_order_status(required_str(value, "status")?)?,
        }),
    })
}

fn parse_user_events(data: &serde_json::Value) -> Result<Vec<UserDataEvent>> {
    if let Some(fills) = data.get("fills").and_then(serde_json::Value::as_array) {
        return fills.iter().map(parse_fill).collect();
    }
    if let Some(funding) = data.get("funding") {
        return parse_funding(funding).map(|event| vec![event]);
    }
    Ok(Vec::new())
}

fn parse_fill(value: &serde_json::Value) -> Result<UserDataEvent> {
    let symbol = required_str(value, "coin")?;
    let time = required_i64(value, "time")?;
    let trade_id = required_u64(value, "tid")?.to_string();
    let order_id = required_u64(value, "oid")?.to_string();
    Ok(UserDataEvent {
        event_time: millis_to_utc(time)?,
        payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
            Instrument::new(Venue::Hyperliquid, symbol),
            millis_to_utc(time)?,
            "hyperliquid:fill".to_string(),
            Some(format!("hyperliquid:fill:{symbol}:{trade_id}")),
            Some(order_id),
            Some(trade_id),
            parse_side(required_str(value, "side")?)?,
            parse_decimal("fill.px", required_str(value, "px")?)?,
            parse_decimal("fill.sz", required_str(value, "sz")?)?,
            parse_decimal("fill.closedPnl", required_str(value, "closedPnl")?)?,
            parse_decimal("fill.fee", required_str(value, "fee")?)?,
        )),
    })
}

fn parse_funding(value: &serde_json::Value) -> Result<UserDataEvent> {
    let symbol = required_str(value, "coin")?;
    let time = required_i64(value, "time")?;
    Ok(UserDataEvent {
        event_time: millis_to_utc(time)?,
        payload: UserDataPayload::TrackPnl(TrackPnlRecord::funding(
            Instrument::new(Venue::Hyperliquid, symbol),
            millis_to_utc(time)?,
            "hyperliquid:funding".to_string(),
            Some(format!("hyperliquid:funding:{symbol}:{time}")),
            parse_decimal("funding.usdc", required_str(value, "usdc")?)?,
        )),
    })
}

fn parse_order_status(value: &str) -> Result<OrderStatus> {
    match value {
        "open" => Ok(OrderStatus::New),
        "filled" => Ok(OrderStatus::Filled),
        "canceled" => Ok(OrderStatus::Canceled),
        "triggered" => Ok(OrderStatus::New),
        "rejected" => Ok(OrderStatus::Rejected),
        "marginCanceled"
        | "openInterestCapCanceled"
        | "selfTradeCanceled"
        | "vaultWithdrawalCanceled" => Ok(OrderStatus::Canceled),
        other => Err(anyhow!("unsupported Hyperliquid order status: {other}")),
    }
}

fn parse_side(value: &str) -> Result<Side> {
    match value {
        "B" => Ok(Side::Buy),
        "A" => Ok(Side::Sell),
        other => Err(anyhow!("unsupported Hyperliquid side: {other}")),
    }
}

fn required_str<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("missing Hyperliquid `{field}`"))
}

fn required_i64(value: &serde_json::Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("missing Hyperliquid `{field}`"))
}

fn required_u64(value: &serde_json::Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("missing Hyperliquid `{field}`"))
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid Hyperliquid decimal `{field}`: {value}"))
}

fn millis_to_utc(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value)
        .ok_or_else(|| anyhow!("invalid Hyperliquid timestamp: {value}"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;
    use poise_engine::ledger::{TrackPnlRecord, TrackPnlRecordKind};
    use poise_engine::ports::{
        ExchangeOrder, ExecutionQuote, MarketDataTick, OrderStatus, UserDataPayload,
    };
    use tokio::net::TcpListener;
    use tokio::time::timeout;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::{
        HyperliquidWsClient, backoff_delay, parse_market_data_message, parse_user_data_message,
    };

    #[test]
    fn parses_bbo_and_active_asset_ctx_into_market_ticks() {
        let bbo = parse_market_data_message(
            r#"{"channel":"bbo","data":{"coin":"BTC","time":1700000000000,"bbo":[{"px":"65000.5","sz":"1.2","n":1},{"px":"65001.0","sz":"0.8","n":1}]}}"#,
            "BTC",
        )
        .unwrap()
        .unwrap();
        let mark = parse_market_data_message(
            r#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{"markPx":"65000.75","midPx":"65000.75","oraclePx":"65000.8","funding":"0.00001","openInterest":"100"}}}"#,
            "BTC",
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            bbo,
            MarketDataTick::ExecutionQuote(poise_engine::ports::ExecutionQuoteTick {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                execution_quote: ExecutionQuote {
                    best_bid: 65000.5,
                    best_ask: 65001.0,
                },
                timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
            })
        );
        assert_eq!(
            mark,
            MarketDataTick::MarkPrice(poise_engine::ports::MarkPriceTick {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                mark_price: 65000.75,
                timestamp: mark_timestamp(),
            })
        );
    }

    #[test]
    fn parses_order_updates_into_user_data_events() {
        let events = parse_user_data_message(
            r#"{"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"B","limitPx":"65000.5","sz":"0.02","oid":12345,"timestamp":1700000000000,"origSz":"0.02","cloid":"0x11111111111111111111111111111111"},"status":"open","statusTimestamp":1700000000001}]}"#,
        )
        .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].payload,
            UserDataPayload::OrderUpdate(ExchangeOrder {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                order_id: "12345".to_string(),
                client_order_id: "0x11111111111111111111111111111111".to_string(),
                side: Side::Buy,
                price: 65000.5,
                qty: 0.02,
                filled_qty: 0.0,
                status: OrderStatus::New,
            })
        );
    }

    #[test]
    fn parses_user_fills_and_funding_into_pnl_events() {
        let fill_events = parse_user_data_message(
            r#"{"channel":"userEvents","data":{"fills":[{"coin":"BTC","px":"65000.5","sz":"0.02","side":"B","time":1700000000000,"closedPnl":"3.25","hash":"0xabc","oid":12345,"tid":999,"fee":"0.12","feeToken":"USDC","crossed":true,"startPosition":"0","dir":"Open Long"}]}}"#,
        )
        .unwrap();
        let funding_events = parse_user_data_message(
            r#"{"channel":"userEvents","data":{"funding":{"time":1700000000000,"coin":"BTC","usdc":"-0.15","szi":"0.02","fundingRate":"0.00001"}}}"#,
        )
        .unwrap();

        assert_eq!(
            fill_events[0].payload,
            UserDataPayload::TrackPnl(TrackPnlRecord {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                occurred_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
                kind: TrackPnlRecordKind::Trade,
                source: "hyperliquid:fill".to_string(),
                source_key: Some("hyperliquid:fill:BTC:999".to_string()),
                order_id: Some("12345".to_string()),
                trade_id: Some("999".to_string()),
                side: Some(Side::Buy),
                price: Some(65000.5),
                qty: Some(0.02),
                realized_pnl: 3.25,
                trading_fee: 0.12,
                funding_fee: 0.0,
            })
        );
        assert_eq!(
            funding_events[0].payload,
            UserDataPayload::TrackPnl(TrackPnlRecord {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                occurred_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
                kind: TrackPnlRecordKind::Funding,
                source: "hyperliquid:funding".to_string(),
                source_key: Some("hyperliquid:funding:BTC:1700000000000".to_string()),
                order_id: None,
                trade_id: None,
                side: None,
                price: None,
                qty: None,
                realized_pnl: 0.0,
                trading_fee: 0.0,
                funding_fee: -0.15,
            })
        );
    }

    #[tokio::test]
    async fn reconnects_market_stream_and_resubscribes_after_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_server = Arc::clone(&observed);

        tokio::spawn(async move {
            for payload in [
                r#"{"channel":"bbo","data":{"coin":"BTC","time":1700000000000,"bbo":[{"px":"65000.5","sz":"1.2","n":1},{"px":"65001.0","sz":"0.8","n":1}]}}"#,
                r#"{"channel":"bbo","data":{"coin":"BTC","time":1700000005000,"bbo":[{"px":"65010.5","sz":"1.2","n":1},{"px":"65011.0","sz":"0.8","n":1}]}}"#,
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                for _ in 0..2 {
                    if let Some(Ok(Message::Text(text))) = websocket.next().await {
                        observed_server.lock().unwrap().push(text);
                    }
                }
                websocket
                    .send(Message::Text(payload.to_string()))
                    .await
                    .unwrap();
                websocket.close(None).await.unwrap();
            }
        });

        let client = HyperliquidWsClient::with_reconnect_delay(
            format!("ws://{address}"),
            "0x2222222222222222222222222222222222222222",
            Duration::from_millis(10),
        );
        let mut receiver = client
            .subscribe_prices(&Instrument::new(Venue::Hyperliquid, "BTC"))
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
        assert_eq!(messages.len(), 4);
        assert!(
            messages
                .iter()
                .filter(|message| message.contains(r#""type":"bbo""#))
                .count()
                == 2
        );
        match first {
            MarketDataTick::ExecutionQuote(tick) => {
                assert_eq!(tick.execution_quote.best_bid, 65000.5);
            }
            MarketDataTick::MarkPrice(_) => panic!("expected execution quote tick"),
        }
        match second {
            MarketDataTick::ExecutionQuote(tick) => {
                assert_eq!(tick.execution_quote.best_bid, 65010.5);
            }
            MarketDataTick::MarkPrice(_) => panic!("expected execution quote tick"),
        }
    }

    #[tokio::test]
    async fn reconnects_user_stream_and_resubscribes_after_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_server = Arc::clone(&observed);

        tokio::spawn(async move {
            for payload in [
                r#"{"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"B","limitPx":"65000.5","sz":"0.02","oid":12345,"timestamp":1700000000000,"origSz":"0.02"},"status":"open","statusTimestamp":1700000000001}]}"#,
                r#"{"channel":"orderUpdates","data":[{"order":{"coin":"BTC","side":"A","limitPx":"65010.5","sz":"0.03","oid":12346,"timestamp":1700000005000,"origSz":"0.03"},"status":"filled","statusTimestamp":1700000005001}]}"#,
            ] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                for _ in 0..2 {
                    if let Some(Ok(Message::Text(text))) = websocket.next().await {
                        observed_server.lock().unwrap().push(text);
                    }
                }
                websocket
                    .send(Message::Text(payload.to_string()))
                    .await
                    .unwrap();
                websocket.close(None).await.unwrap();
            }
        });

        let client = HyperliquidWsClient::with_reconnect_delay(
            format!("ws://{address}"),
            "0x2222222222222222222222222222222222222222",
            Duration::from_millis(10),
        );
        let mut receiver = client.subscribe_user_data().await.unwrap();
        let first = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();

        let messages = observed.lock().unwrap();
        assert_eq!(messages.len(), 4);
        assert!(
            messages
                .iter()
                .filter(|message| message.contains(r#""type":"orderUpdates""#))
                .count()
                == 2
        );
        assert_eq!(first.event_time.timestamp_millis(), 1_700_000_000_001);
        assert_eq!(second.event_time.timestamp_millis(), 1_700_000_005_001);
    }

    #[test]
    fn backoff_delay_caps_after_four_attempts() {
        assert_eq!(
            backoff_delay(Duration::from_millis(10), 0),
            Duration::from_millis(10)
        );
        assert_eq!(
            backoff_delay(Duration::from_millis(10), 4),
            Duration::from_millis(160)
        );
        assert_eq!(
            backoff_delay(Duration::from_millis(10), 10),
            Duration::from_millis(160)
        );
    }

    fn mark_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(0).unwrap()
    }
}
