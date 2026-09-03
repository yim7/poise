use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use chrono::{TimeZone, Utc};
use futures_util::StreamExt;
use tokio::{
    sync::mpsc,
    time::{Duration, Instant, interval_at, sleep},
};
use tokio_tungstenite::tungstenite::Message;

use poise_application::exchange::TrackPnlRecord;
use poise_application::exchange::{ExchangeOrder, Position, UserDataEvent, UserDataPayload};
use poise_core::instrument::{Amount, Asset, PositionQty};
use poise_core::track::{Instrument, Venue};

use super::{
    EVENT_ACCOUNT_UPDATE, EVENT_LISTEN_KEY_EXPIRED, EVENT_ORDER_TRADE_UPDATE, KeepaliveStatus,
    UserStreamDiagnostics, UserWebSocket, backoff_delay, connect_user_stream,
    log_user_stream_disconnect, log_user_stream_error,
    models::{AccountUpdateEnvelope, OrderTradeUpdateEnvelope, UserEventTypeEnvelope},
    parse_decimal, parse_side,
};
use crate::{
    instrument::BinanceInstrumentRegistry,
    mapper::{parse_order_status, pnl_quantity},
    rest::BinanceRestClient,
};

#[derive(Debug, PartialEq)]
pub(super) enum UserStreamMessage {
    Events(Vec<UserDataEvent>),
    ListenKeyExpired,
    UnsupportedEvent { event_type: String },
}

pub(super) async fn run_user_stream(
    ws_base_url: String,
    rest: Arc<BinanceRestClient>,
    initial_listen_key: String,
    mut initial_websocket: Option<UserWebSocket>,
    sender: mpsc::Sender<UserDataEvent>,
    reconnect_delay: Duration,
) {
    let mut attempt = 0_u32;
    let mut listen_key = initial_listen_key;
    let instrument_registry = rest.instrument_registry();

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
                                    match parse_user_data_message(&text, &instrument_registry) {
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
                                        Ok(UserStreamMessage::UnsupportedEvent { event_type }) => {
                                            tracing::warn!(
                                                event_type = %event_type,
                                                "ignoring unsupported user data event"
                                            );
                                        }
                                        Err(error) => {
                                            tracing::warn!("failed to parse user data message: {error:#}");
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

pub(super) fn parse_user_data_message(
    payload: &str,
    instrument_registry: &BinanceInstrumentRegistry,
) -> Result<UserStreamMessage> {
    let event_type = serde_json::from_str::<UserEventTypeEnvelope>(payload)
        .context("failed to parse user data event type")?
        .event_type;

    match event_type.as_str() {
        EVENT_ORDER_TRADE_UPDATE => {
            let envelope = serde_json::from_str::<OrderTradeUpdateEnvelope>(payload)
                .with_context(|| format!("failed to parse {event_type} payload"))?;
            let event_time = parse_user_event_time(&event_type, envelope.event_time)?;
            let order = envelope.order;
            let price = parse_decimal("o.p", &order.price)?;
            let quantity = parse_decimal("o.q", &order.quantity)?;
            let filled_qty = order
                .cumulative_filled_quantity
                .as_deref()
                .map(|value| parse_decimal("o.z", value))
                .transpose()?
                .unwrap_or(0.0);
            let instrument = Instrument::new(Venue::Binance, order.symbol.clone());
            let side = parse_side(&order.side)?;

            let mut events = vec![UserDataEvent {
                event_time,
                payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                    instrument: instrument.clone(),
                    order_id: order.order_id.to_string(),
                    client_order_id: order.client_order_id.clone(),
                    side,
                    price,
                    qty: PositionQty::new(quantity).map_err(anyhow::Error::msg)?,
                    filled_qty: PositionQty::new(filled_qty).map_err(anyhow::Error::msg)?,
                    status: parse_order_status(&order.status)?,
                }),
            }];

            if order.execution_type.as_deref() == Some("TRADE") {
                let trade_qty = order
                    .last_filled_quantity
                    .as_deref()
                    .context("missing o.l for Binance trade")
                    .and_then(|value| parse_decimal("o.l", value))?;
                if trade_qty.abs() <= f64::EPSILON {
                    return Ok(UserStreamMessage::Events(events));
                }
                let instrument_spec =
                    instrument_registry.get(&order.symbol).with_context(|| {
                        format!(
                            "missing Binance instrument metadata for trade symbol `{}`",
                            order.symbol
                        )
                    })?;
                let realized_pnl = parse_decimal("o.rp", &order.realized_pnl)?;
                let trade_price = order
                    .last_filled_price
                    .as_deref()
                    .context("missing o.L for Binance trade")
                    .and_then(|value| parse_decimal("o.L", value))?;
                let occurred_at = parse_user_event_time(
                    &event_type,
                    order
                        .order_trade_time
                        .context("missing o.T for Binance trade")?,
                )?;
                let margin_asset = order
                    .margin_asset
                    .as_deref()
                    .map(str::trim)
                    .filter(|asset| !asset.is_empty())
                    .context("missing o.ma for Binance trade")?;
                ensure!(
                    margin_asset == instrument_spec.settlement_asset().as_str(),
                    "Binance trade margin asset `{margin_asset}` does not match settlement asset `{}` for `{}`",
                    instrument_spec.settlement_asset().as_str(),
                    order.symbol
                );
                let commission_amount = order
                    .commission_amount
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .context("missing o.n for Binance trade")
                    .and_then(|value| parse_decimal("o.n", value))?;
                let commission_asset = order
                    .commission_asset
                    .as_deref()
                    .map(str::trim)
                    .filter(|asset| !asset.is_empty())
                    .context("missing o.N for Binance trade")?;
                let trading_fee = Some(cash_amount(commission_amount, commission_asset)?);
                let trade_id = order.trade_id.map(|value| value.to_string());
                let source_key = Some(match trade_id.as_deref() {
                    Some(trade_id) => format!(
                        "binance:order_trade_update:{}:{}",
                        order.symbol.to_lowercase(),
                        trade_id
                    ),
                    None => format!(
                        "binance:order_trade_update:{}:{}:{}",
                        order.symbol.to_lowercase(),
                        order.order_id,
                        occurred_at.timestamp_millis()
                    ),
                });

                events.push(UserDataEvent {
                    event_time,
                    payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                        instrument,
                        occurred_at,
                        "binance:order_trade_update".into(),
                        source_key,
                        Some(order.order_id.to_string()),
                        trade_id,
                        side,
                        trade_price,
                        pnl_quantity(
                            PositionQty::new(trade_qty).map_err(anyhow::Error::msg)?,
                            &instrument_spec,
                        )?,
                        cash_amount(realized_pnl, margin_asset)?,
                        trading_fee,
                    )),
                });
            }

            Ok(UserStreamMessage::Events(events))
        }
        EVENT_ACCOUNT_UPDATE => {
            let envelope = serde_json::from_str::<AccountUpdateEnvelope>(payload)
                .with_context(|| format!("failed to parse {event_type} payload"))?;
            let event_time = parse_user_event_time(&event_type, envelope.event_time)?;
            let transaction_time = parse_user_event_time(&event_type, envelope.transaction_time)?;
            let account = envelope.account;
            if account.reason.as_deref() == Some("FUNDING_FEE") {
                let Some(balance) = account.balances.iter().find(|balance| {
                    balance.balance_change != "0" && balance.balance_change != "0.0"
                }) else {
                    return Ok(UserStreamMessage::Events(Vec::new()));
                };
                let symbol = account
                    .funding_symbol
                    .as_deref()
                    .map(str::trim)
                    .filter(|symbol| !symbol.is_empty())
                    .or_else(|| {
                        account
                            .positions
                            .iter()
                            .map(|position| position.symbol.as_str())
                            .find(|symbol| !symbol.is_empty())
                    })
                    .context("missing symbol for Binance funding fee")?;
                let balance_change = parse_decimal("a.B.bc", &balance.balance_change)?;
                let instrument_spec = instrument_registry.get(symbol).with_context(|| {
                    format!("missing Binance instrument metadata for funding symbol `{symbol}`")
                })?;
                ensure!(
                    balance.asset == instrument_spec.settlement_asset().as_str(),
                    "Binance funding asset `{}` does not match settlement asset `{}` for `{symbol}`",
                    balance.asset,
                    instrument_spec.settlement_asset().as_str()
                );

                return Ok(UserStreamMessage::Events(vec![UserDataEvent {
                    event_time,
                    payload: UserDataPayload::TrackPnl(TrackPnlRecord::funding(
                        Instrument::new(Venue::Binance, symbol.to_string()),
                        transaction_time,
                        "binance:funding_fee".into(),
                        Some(format!(
                            "binance:funding_fee:{}:{}",
                            symbol.to_lowercase(),
                            transaction_time.timestamp_millis()
                        )),
                        cash_amount(balance_change, &balance.asset)?,
                    )),
                }]));
            }

            let events = account
                .positions
                .into_iter()
                .map(|position| {
                    Ok(UserDataEvent {
                        event_time,
                        payload: UserDataPayload::PositionUpdate(Position {
                            instrument: Instrument::new(Venue::Binance, position.symbol),
                            qty: PositionQty::new(parse_decimal("a.P.pa", &position.position_amt)?)
                                .map_err(anyhow::Error::msg)?,
                            avg_price: parse_decimal("a.P.ep", &position.entry_price)?,
                            unrealized_pnl: parse_decimal("a.P.up", &position.unrealized_profit)?,
                            mark_price: None,
                        }),
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            Ok(UserStreamMessage::Events(events))
        }
        EVENT_LISTEN_KEY_EXPIRED => Ok(UserStreamMessage::ListenKeyExpired),
        _ => Ok(UserStreamMessage::UnsupportedEvent { event_type }),
    }
}

fn cash_amount(value: f64, asset: &str) -> Result<Amount> {
    let asset = Asset::try_new(asset).map_err(anyhow::Error::msg)?;
    Amount::new(value, asset).map_err(anyhow::Error::msg)
}

fn parse_user_event_time(event_type: &str, event_time: i64) -> Result<chrono::DateTime<Utc>> {
    Utc.timestamp_millis_opt(event_time)
        .single()
        .with_context(|| format!("invalid {event_type} timestamp"))
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

    use poise_application::exchange::{OrderStatus, Position, TrackPnlQuantity, UserDataPayload};
    use poise_core::instrument::InstrumentSpec;
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;

    use super::*;
    use crate::ws::BinanceWsClient;

    fn test_registry() -> BinanceInstrumentRegistry {
        let registry = BinanceInstrumentRegistry::default();
        registry.upsert(
            InstrumentSpec::try_linear(
                Instrument::new(Venue::Binance, "BTCUSDT"),
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
        registry.upsert(
            InstrumentSpec::try_linear(
                Instrument::new(Venue::Binance, "PERPETUAL-42"),
                Asset::try_new("BTC").unwrap(),
                Asset::try_new("USD").unwrap(),
                Asset::try_new("USDC").unwrap(),
                Default::default(),
                0.1,
                0.001,
                0.001,
                Amount::new(5.0, Asset::try_new("USD").unwrap()).unwrap(),
            )
            .unwrap(),
        );
        registry
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
                "x": "TRADE",
                "p": "65000.5",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65000.5",
                "T": 1700000000000,
                "rp": "12.34",
                "n": "0",
                "N": "USDT",
                "ma": "USDT",
                "X": "FILLED"
            }
        }"#;

        let registry = test_registry();
        let events = parse_user_data_message(payload, &registry).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        order_id: "12345".into(),
                        client_order_id: "grid-order-004".into(),
                        side: Side::Sell,
                        price: 65000.5,
                        qty: PositionQty::new(0.02).unwrap(),
                        filled_qty: PositionQty::new(0.02).unwrap(),
                        status: OrderStatus::Filled,
                    }),
                },
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                        Instrument::new(Venue::Binance, "BTCUSDT"),
                        Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                        "binance:order_trade_update".into(),
                        Some("binance:order_trade_update:btcusdt:12345:1700000000000".into()),
                        Some("12345".into()),
                        None,
                        Side::Sell,
                        65000.5,
                        TrackPnlQuantity::base_asset(
                            PositionQty::new(0.02).unwrap(),
                            Asset::try_new("BTC").unwrap(),
                        ),
                        cash_amount(12.34, "USDT").unwrap(),
                        Some(cash_amount(0.0, "USDT").unwrap()),
                    )),
                },
            ])
        );
    }

    #[test]
    fn parses_order_trade_update_into_order_update_and_pnl_record() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "x": "TRADE",
                "p": "65000.5",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65001.5",
                "T": 1699999999123,
                "t": 67890,
                "rp": "12.34",
                "n": "3.2",
                "N": "USDT",
                "ma": "USDT",
                "X": "FILLED"
            }
        }"#;

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        order_id: "12345".into(),
                        client_order_id: "grid-order-004".into(),
                        side: Side::Sell,
                        price: 65000.5,
                        qty: PositionQty::new(0.02).unwrap(),
                        filled_qty: PositionQty::new(0.02).unwrap(),
                        status: OrderStatus::Filled,
                    }),
                },
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                        Instrument::new(Venue::Binance, "BTCUSDT"),
                        Utc.timestamp_millis_opt(1_699_999_999_123).unwrap(),
                        "binance:order_trade_update".into(),
                        Some("binance:order_trade_update:btcusdt:67890".into()),
                        Some("12345".into()),
                        Some("67890".into()),
                        Side::Sell,
                        65001.5,
                        TrackPnlQuantity::base_asset(
                            PositionQty::new(0.02).unwrap(),
                            Asset::try_new("BTC").unwrap(),
                        ),
                        cash_amount(12.34, "USDT").unwrap(),
                        Some(cash_amount(3.2, "USDT").unwrap()),
                    )),
                },
            ])
        );
    }

    #[test]
    fn ws_and_rest_trades_share_metadata_quantity_for_nonstandard_symbol() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "PERPETUAL-42",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "x": "TRADE",
                "p": "0",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65001.5",
                "T": 1699999999123,
                "t": 67890,
                "rp": "12.34",
                "n": "0.01",
                "N": "BNB",
                "ma": "USDC",
                "X": "FILLED"
            }
        }"#;
        let registry = test_registry();

        let UserStreamMessage::Events(events) =
            parse_user_data_message(payload, &registry).unwrap()
        else {
            panic!("trade update must emit events");
        };
        let UserDataPayload::TrackPnl(streamed) = &events[1].payload else {
            panic!("trade update must emit PNL");
        };
        let backfilled = crate::mapper::track_pnl_record_from_trade(
            crate::rest::models::BinanceAccountTrade {
                commission: "0.01".into(),
                commission_asset: "BNB".into(),
                id: 67890,
                order_id: 12345,
                price: "65001.5".into(),
                qty: "0.020".into(),
                margin_asset: "USDC".into(),
                realized_pnl: "12.34".into(),
                side: "SELL".into(),
                symbol: "PERPETUAL-42".into(),
                time: 1_699_999_999_123,
            },
            &registry.get("PERPETUAL-42").unwrap(),
        )
        .unwrap();

        assert_eq!(streamed.occurred_at, backfilled.occurred_at);
        assert_eq!(streamed.price, Some(65_001.5));
        assert_eq!(streamed.quantity, backfilled.quantity);
        assert_eq!(streamed.realized_pnl().unwrap().asset().as_str(), "USDC");
        assert_eq!(streamed.trading_fee().unwrap().asset().as_str(), "BNB");
    }

    #[test]
    fn rejects_trade_when_instrument_metadata_is_missing() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "x": "TRADE",
                "p": "65000.5",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65000.5",
                "T": 1700000000000,
                "t": 67890,
                "rp": "12.34",
                "ma": "USDT",
                "X": "FILLED"
            }
        }"#;

        let error = parse_user_data_message(payload, &BinanceInstrumentRegistry::default())
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing Binance instrument metadata"));
        assert!(error.contains("BTCUSDT"));
    }

    #[test]
    fn rejects_trade_when_commission_asset_is_missing() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "x": "TRADE",
                "p": "65000.5",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65000.5",
                "T": 1700000000000,
                "t": 67890,
                "rp": "12.34",
                "n": "0.01",
                "ma": "USDT",
                "X": "FILLED"
            }
        }"#;

        let error = parse_user_data_message(payload, &test_registry())
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing o.N"));
    }

    #[test]
    fn rejects_nonzero_funding_fee_without_symbol() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "T": 1699999999000,
            "a": {
                "m": "FUNDING_FEE",
                "B": [{
                    "a": "USDT",
                    "bc": "-1.5"
                }],
                "P": []
            }
        }"#;

        let error = parse_user_data_message(payload, &test_registry())
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing symbol for Binance funding fee"));
    }

    #[test]
    fn funding_symbol_field_attributes_fee_without_position() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "T": 1699999999000,
            "a": {
                "m": "FUNDING_FEE",
                "S": "BTCUSDT",
                "B": [{
                    "a": "USDT",
                    "bc": "-1.5"
                }],
                "P": []
            }
        }"#;

        let UserStreamMessage::Events(events) =
            parse_user_data_message(payload, &test_registry()).unwrap()
        else {
            panic!("funding update must emit events");
        };
        let UserDataPayload::TrackPnl(record) = &events[0].payload else {
            panic!("funding update must emit PNL");
        };

        assert_eq!(record.instrument.symbol, "BTCUSDT");
        assert_eq!(record.funding_amount().unwrap().asset().as_str(), "USDT");
    }

    #[test]
    fn rejects_funding_fee_when_balance_asset_does_not_match_instrument() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "T": 1699999999000,
            "a": {
                "m": "FUNDING_FEE",
                "B": [{
                    "a": "BNB",
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

        let error = parse_user_data_message(payload, &test_registry())
            .unwrap_err()
            .to_string();

        assert!(error.contains("funding asset"));
        assert!(error.contains("BNB"));
        assert!(error.contains("BTCUSDT"));
    }

    #[test]
    fn parses_funding_fee_account_update_into_track_pnl_record() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "T": 1699999999000,
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

        let registry = test_registry();
        let events = parse_user_data_message(payload, &registry).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::TrackPnl(TrackPnlRecord::funding(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    Utc.timestamp_millis_opt(1_699_999_999_000).unwrap(),
                    "binance:funding_fee".into(),
                    Some("binance:funding_fee:btcusdt:1699999999000".into()),
                    cash_amount(-1.5, "USDT").unwrap(),
                )),
            }])
        );

        let backfilled = crate::mapper::track_pnl_record_from_funding(
            crate::rest::models::BinanceIncomeRecord {
                symbol: "BTCUSDT".into(),
                income_type: "FUNDING_FEE".into(),
                income: "-1.5".into(),
                asset: "USDT".into(),
                time: 1_699_999_999_000,
            },
            &registry.get("BTCUSDT").unwrap(),
        )
        .unwrap();
        let UserStreamMessage::Events(events) = events else {
            panic!("funding account update must emit an event");
        };
        let UserDataPayload::TrackPnl(streamed) = &events[0].payload else {
            panic!("funding account update must emit a PNL record");
        };
        assert_eq!(streamed.source_key, backfilled.source_key);
        assert_eq!(streamed.occurred_at, backfilled.occurred_at);
    }

    #[test]
    fn non_trade_order_update_does_not_create_pnl_record() {
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
                "z": "0.000",
                "l": "0.000",
                "x": "NEW",
                "rp": "0",
                "X": "NEW"
            }
        }"#;

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    order_id: "12345".into(),
                    client_order_id: "grid-order-004".into(),
                    side: Side::Sell,
                    price: 65000.5,
                    qty: PositionQty::new(0.02).unwrap(),
                    filled_qty: PositionQty::default(),
                    status: OrderStatus::New,
                }),
            }])
        );
    }

    #[test]
    fn preserves_non_settlement_commission_asset_in_pnl_record() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "i": 12345,
                "c": "grid-order-004",
                "S": "SELL",
                "x": "TRADE",
                "p": "65000.5",
                "q": "0.020",
                "z": "0.020",
                "l": "0.020",
                "L": "65000.5",
                "T": 1700000000000,
                "rp": "12.34",
                "n": "0.01",
                "N": "BNB",
                "ma": "USDT",
                "X": "FILLED"
            }
        }"#;

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::OrderUpdate(ExchangeOrder {
                        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                        order_id: "12345".into(),
                        client_order_id: "grid-order-004".into(),
                        side: Side::Sell,
                        price: 65000.5,
                        qty: PositionQty::new(0.02).unwrap(),
                        filled_qty: PositionQty::new(0.02).unwrap(),
                        status: OrderStatus::Filled,
                    }),
                },
                UserDataEvent {
                    event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                    payload: UserDataPayload::TrackPnl(TrackPnlRecord::trade(
                        Instrument::new(Venue::Binance, "BTCUSDT"),
                        Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                        "binance:order_trade_update".into(),
                        Some("binance:order_trade_update:btcusdt:12345:1700000000000".into()),
                        Some("12345".into()),
                        None,
                        Side::Sell,
                        65000.5,
                        TrackPnlQuantity::base_asset(
                            PositionQty::new(0.02).unwrap(),
                            Asset::try_new("BTC").unwrap(),
                        ),
                        cash_amount(12.34, "USDT").unwrap(),
                        Some(cash_amount(0.01, "BNB").unwrap()),
                    )),
                },
            ])
        );
    }

    #[test]
    fn parses_account_update_message() {
        let payload = r#"{
            "e": "ACCOUNT_UPDATE",
            "E": 1700000000000,
            "T": 1700000000000,
            "a": {
                "P": [{
                    "s": "BTCUSDT",
                    "pa": "0.015",
                    "ep": "64200.0",
                    "cr": "200.5",
                    "up": "12.3"
                }]
            }
        }"#;

        let events = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            events,
            UserStreamMessage::Events(vec![UserDataEvent {
                event_time: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                payload: UserDataPayload::PositionUpdate(Position {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    qty: PositionQty::new(0.015).unwrap(),
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                    mark_price: None,
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

        let event = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(event, UserStreamMessage::ListenKeyExpired);
    }

    #[test]
    fn surfaces_algo_update_message_type() {
        let payload = r#"{
            "e": "ALGO_UPDATE",
            "E": 1750515742303,
            "T": 1750515742297,
            "o": {
                "caid": "Q5xaq5EGKgXXa0fD7fs0Ip",
                "aid": 2148719,
                "at": "CONDITIONAL",
                "o": "TAKE_PROFIT",
                "s": "BNBUSDT",
                "S": "SELL",
                "ps": "BOTH",
                "f": "GTC",
                "q": "0.01",
                "X": "CANCELED",
                "ai": "",
                "ap": "0.00000",
                "aq": "0.00000",
                "act": "0",
                "tp": "750",
                "p": "750",
                "V": "EXPIRE_MAKER",
                "wt": "CONTRACT_PRICE",
                "pm": "NONE",
                "cp": false,
                "pP": false,
                "R": false,
                "tt": 0,
                "gtd": 0,
                "rm": "Reduce Only reject"
            }
        }"#;

        let event = parse_user_data_message(payload, &test_registry()).unwrap();

        assert_eq!(
            event,
            UserStreamMessage::UnsupportedEvent {
                event_type: "ALGO_UPDATE".into(),
            }
        );
    }

    #[test]
    fn malformed_order_trade_update_error_mentions_event_type() {
        let payload = r#"{
            "e": "ORDER_TRADE_UPDATE",
            "E": 1700000000000,
            "o": {
                "s": "BTCUSDT",
                "c": "grid-order-004",
                "S": "SELL",
                "p": "65000.5",
                "q": "0.020",
                "rp": "12.34",
                "X": "FILLED"
            }
        }"#;

        let error = parse_user_data_message(payload, &test_registry()).unwrap_err();
        let error_text = format!("{error:#}");

        assert!(error_text.contains("ORDER_TRADE_UPDATE"));
        assert!(error_text.contains("missing field `i`"));
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
                        r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"T":1700000000000,"a":{"P":[{"s":"BTCUSDT","pa":"0.015","ep":"64200.0","up":"12.3"}]}}"#
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
                    qty: PositionQty::new(0.015).unwrap(),
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                    mark_price: None,
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
                        r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"T":1700000000000,"a":{"P":[{"s":"BTCUSDT","pa":"0.015","ep":"64200.0","up":"12.3"}]}}"#
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
                    qty: PositionQty::new(0.015).unwrap(),
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                    mark_price: None,
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
                        r#"{"e":"ACCOUNT_UPDATE","E":1700000000000,"T":1700000000000,"a":{"P":[{"s":"BTCUSDT","pa":"0.015","ep":"64200.0","up":"12.3"}]}}"#
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
                    qty: PositionQty::new(0.015).unwrap(),
                    avg_price: 64200.0,
                    unrealized_pnl: 12.3,
                    mark_price: None,
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
