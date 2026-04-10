use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};

use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderStatus, Position,
};
use poise_engine::track::{Instrument, Venue};

use crate::rest::models::{
    CreateOrderResult, InstrumentInfoResult, OpenOrderSnapshot, PositionSnapshot, ServerTimeResult,
    UnifiedWalletBalance, WalletBalanceResult,
};

pub(crate) fn build_account_capacity_snapshot(
    wallet_balance: &WalletBalanceResult,
) -> Result<AccountCapacitySnapshot> {
    let balance = first_wallet_balance(wallet_balance)?;
    let available = required_decimal(
        "totalAvailableBalance",
        balance.total_available_balance.as_deref(),
    )?;
    Ok(AccountCapacitySnapshot {
        max_increase_notional: available,
    })
}

impl TryFrom<InstrumentInfoResult> for ExchangeInfo {
    type Error = anyhow::Error;

    fn try_from(value: InstrumentInfoResult) -> Result<Self, Self::Error> {
        let info = value
            .list
            .into_iter()
            .next()
            .context("missing linear instrument info")?;
        Ok(Self {
            instrument: Instrument::new(Venue::Bybit, info.symbol),
            rules: poise_core::types::ExchangeRules {
                price_tick: required_decimal(
                    "priceFilter.tickSize",
                    info.price_filter.tick_size.as_deref(),
                )?,
                quantity_step: required_decimal(
                    "lotSizeFilter.qtyStep",
                    info.lot_size_filter.qty_step.as_deref(),
                )?,
                min_qty: required_decimal(
                    "lotSizeFilter.minOrderQty",
                    info.lot_size_filter.min_order_qty.as_deref(),
                )?,
                min_notional: required_decimal(
                    "lotSizeFilter.minNotionalValue",
                    info.lot_size_filter.min_notional_value.as_deref(),
                )?,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.00055,
            },
        })
    }
}

impl WalletBalanceResult {
    pub(crate) fn into_account_summary_snapshot(self) -> Result<AccountSummarySnapshot> {
        let balance = first_wallet_balance(&self)?;
        Ok(AccountSummarySnapshot {
            equity: required_decimal("totalEquity", balance.total_equity.as_deref())?,
            available: required_decimal(
                "totalAvailableBalance",
                balance.total_available_balance.as_deref(),
            )?,
            unrealized_pnl: required_decimal("totalPerpUPL", balance.total_perp_upl.as_deref())?,
            observed_at: Utc::now(),
        })
    }
}

impl TryFrom<ServerTimeResult> for chrono::DateTime<Utc> {
    type Error = anyhow::Error;

    fn try_from(value: ServerTimeResult) -> Result<Self, Self::Error> {
        Utc.timestamp_opt(value.time_second, 0)
            .single()
            .ok_or_else(|| anyhow!("invalid Bybit server time: {}", value.time_second))
    }
}

impl TryFrom<CreateOrderResult> for OrderReceipt {
    type Error = anyhow::Error;

    fn try_from(value: CreateOrderResult) -> Result<Self, Self::Error> {
        Ok(Self {
            order_id: value.order_id,
            client_order_id: value.order_link_id.unwrap_or_default(),
            status: OrderStatus::Submitting,
        })
    }
}

impl TryFrom<PositionSnapshot> for Position {
    type Error = anyhow::Error;

    fn try_from(value: PositionSnapshot) -> Result<Self, Self::Error> {
        build_bybit_position(
            value.symbol,
            value.side.as_deref(),
            &value.size,
            &value.avg_price,
            &value.unrealised_pnl,
            value.position_idx,
        )
    }
}

impl TryFrom<OpenOrderSnapshot> for ExchangeOrder {
    type Error = anyhow::Error;

    fn try_from(value: OpenOrderSnapshot) -> Result<Self, Self::Error> {
        build_bybit_open_order(
            value.symbol,
            value.order_id,
            value.order_link_id,
            &value.side,
            &value.price,
            &value.qty,
            &value.order_status,
            value.position_idx,
        )
    }
}

fn first_wallet_balance(wallet_balance: &WalletBalanceResult) -> Result<&UnifiedWalletBalance> {
    let balance = wallet_balance
        .list
        .first()
        .context("missing unified wallet balance entry")?;
    if balance.account_type.as_deref() != Some("UNIFIED") {
        return Err(anyhow!(
            "expected wallet balance accountType=UNIFIED, got {}",
            balance.account_type.as_deref().unwrap_or("missing")
        ));
    }
    Ok(balance)
}

fn required_decimal(field: &str, value: Option<&str>) -> Result<f64> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field}"))?;
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

pub(crate) fn parse_side(value: &str) -> Result<poise_core::types::Side> {
    match value {
        "Buy" | "BUY" | "buy" => Ok(poise_core::types::Side::Buy),
        "Sell" | "SELL" | "sell" => Ok(poise_core::types::Side::Sell),
        other => Err(anyhow!("unsupported Bybit side: {other}")),
    }
}

pub(crate) fn side_to_bybit(side: poise_core::types::Side) -> &'static str {
    match side {
        poise_core::types::Side::Buy => "Buy",
        poise_core::types::Side::Sell => "Sell",
    }
}

pub(crate) fn parse_order_status(value: &str) -> Result<OrderStatus> {
    match value {
        "New" | "NEW" => Ok(OrderStatus::New),
        "PartiallyFilled" | "PARTIALLY_FILLED" => Ok(OrderStatus::PartiallyFilled),
        "Filled" | "FILLED" => Ok(OrderStatus::Filled),
        "Cancelled" | "CANCELED" => Ok(OrderStatus::Canceled),
        "Rejected" | "REJECTED" => Ok(OrderStatus::Rejected),
        "Expired" | "EXPIRED" => Ok(OrderStatus::Expired),
        other => Err(anyhow!("unsupported Bybit order status: {other}")),
    }
}

pub(crate) fn build_bybit_position(
    symbol: String,
    side: Option<&str>,
    size: &str,
    avg_price: &str,
    unrealised_pnl: &str,
    position_idx: i64,
) -> Result<Position> {
    if position_idx != 0 {
        return Err(anyhow!(
            "Bybit one-way position snapshot requires positionIdx=0, got {position_idx}"
        ));
    }

    let qty = parse_decimal("size", size)?;
    let side_multiplier = match side {
        Some("Buy") | Some("BUY") | Some("buy") | None => 1.0,
        Some("Sell") | Some("SELL") | Some("sell") => -1.0,
        Some(other) => return Err(anyhow!("unsupported Bybit side: {other}")),
    };

    Ok(Position {
        instrument: Instrument::new(Venue::Bybit, symbol),
        qty: qty * side_multiplier,
        avg_price: parse_decimal("avgPrice", avg_price)?,
        unrealized_pnl: parse_decimal("unrealisedPnl", unrealised_pnl)?,
    })
}

pub(crate) fn build_bybit_open_order(
    symbol: String,
    order_id: String,
    client_order_id: Option<String>,
    side: &str,
    price: &str,
    qty: &str,
    order_status: &str,
    position_idx: i64,
) -> Result<ExchangeOrder> {
    if position_idx != 0 {
        return Err(anyhow!(
            "Bybit one-way order snapshot requires positionIdx=0, got {position_idx}"
        ));
    }

    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Bybit, symbol),
        order_id,
        client_order_id: client_order_id.unwrap_or_default(),
        side: parse_side(side)?,
        price: parse_decimal("price", price)?,
        qty: parse_decimal("qty", qty)?,
        realized_pnl: 0.0,
        status: parse_order_status(order_status)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::models::{
        CreateOrderResult, InstrumentInfoResult, LinearInstrumentInfo, LotSizeFilter,
        OpenOrderSnapshot, PositionSnapshot, PriceFilter, ServerTimeResult, UnifiedWalletBalance,
        WalletBalanceResult,
    };
    use poise_core::types::{ExchangeRules, Side};
    use poise_engine::ports::{OrderReceipt, OrderStatus, Position};

    #[test]
    fn converts_linear_instrument_info_into_exchange_info() {
        let info = LinearInstrumentInfo {
            symbol: "BTCUSDT".to_string(),
            price_filter: PriceFilter {
                tick_size: Some("0.10".to_string()),
            },
            lot_size_filter: LotSizeFilter {
                qty_step: Some("0.001".to_string()),
                min_order_qty: Some("0.001".to_string()),
                min_notional_value: Some("5".to_string()),
            },
        };

        let exchange_info =
            ExchangeInfo::try_from(InstrumentInfoResult { list: vec![info] }).unwrap();

        assert_eq!(
            exchange_info.instrument,
            Instrument::new(Venue::Bybit, "BTCUSDT")
        );
        assert_eq!(
            exchange_info.rules,
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.001,
                min_qty: 0.001,
                min_notional: 5.0,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.00055,
            }
        );
    }

    #[test]
    fn converts_unified_wallet_balance_into_account_summary_snapshot() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let snapshot = balances.into_account_summary_snapshot().unwrap();

        assert_eq!(snapshot.equity, 125.5);
        assert_eq!(snapshot.available, 100.25);
        assert_eq!(snapshot.unrealized_pnl, -2.75);
    }

    #[test]
    fn missing_unified_wallet_balance_fields_fail_stably() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: None,
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let error = balances
            .into_account_summary_snapshot()
            .unwrap_err()
            .to_string();

        assert!(error.contains("totalEquity"));
    }

    #[test]
    fn non_unified_wallet_balance_entries_fail_stably() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("CONTRACT".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let error = balances
            .into_account_summary_snapshot()
            .unwrap_err()
            .to_string();

        assert!(error.contains("accountType=UNIFIED"));
    }

    #[test]
    fn builds_account_capacity_snapshot_from_available_balance_only() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let snapshot = build_account_capacity_snapshot(&balances).unwrap();

        assert_eq!(snapshot.max_increase_notional, 100.25);
    }

    #[test]
    fn parses_bybit_server_time() {
        let response = ServerTimeResult {
            time_second: 1_700_000_000,
        };

        let time = chrono::DateTime::<Utc>::try_from(response).unwrap();

        assert_eq!(time.timestamp(), 1_700_000_000);
    }

    #[test]
    fn converts_create_order_response_into_order_receipt() {
        let receipt = OrderReceipt::try_from(CreateOrderResult {
            order_id: "12345".to_string(),
            order_link_id: Some("client-1".to_string()),
        })
        .unwrap();

        assert_eq!(
            receipt,
            OrderReceipt {
                order_id: "12345".to_string(),
                client_order_id: "client-1".to_string(),
                status: OrderStatus::Submitting,
            }
        );
    }

    #[test]
    fn converts_one_way_position_snapshot_into_position() {
        let position = Position::try_from(PositionSnapshot {
            symbol: "BTCUSDT".to_string(),
            side: Some("Sell".to_string()),
            size: "0.25".to_string(),
            avg_price: "65000.5".to_string(),
            unrealised_pnl: "-12.5".to_string(),
            position_idx: 0,
        })
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                qty: -0.25,
                avg_price: 65000.5,
                unrealized_pnl: -12.5,
            }
        );
    }

    #[test]
    fn rejects_non_one_way_position_snapshot() {
        let error = Position::try_from(PositionSnapshot {
            symbol: "BTCUSDT".to_string(),
            side: Some("Buy".to_string()),
            size: "0.25".to_string(),
            avg_price: "65000.5".to_string(),
            unrealised_pnl: "-12.5".to_string(),
            position_idx: 1,
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("positionIdx=0"));
    }

    #[test]
    fn converts_open_order_snapshot_into_exchange_order() {
        let order = ExchangeOrder::try_from(OpenOrderSnapshot {
            symbol: "BTCUSDT".to_string(),
            order_id: "12345".to_string(),
            order_link_id: Some("client-1".to_string()),
            side: "Buy".to_string(),
            price: "65000.5".to_string(),
            qty: "0.25".to_string(),
            order_status: "PartiallyFilled".to_string(),
            position_idx: 0,
        })
        .unwrap();

        assert_eq!(order.instrument, Instrument::new(Venue::Bybit, "BTCUSDT"));
        assert_eq!(order.client_order_id, "client-1");
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.price, 65000.5);
        assert_eq!(order.qty, 0.25);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn rejects_non_one_way_open_order_snapshot() {
        let error = ExchangeOrder::try_from(OpenOrderSnapshot {
            symbol: "BTCUSDT".to_string(),
            order_id: "12345".to_string(),
            order_link_id: Some("client-1".to_string()),
            side: "Buy".to_string(),
            price: "65000.5".to_string(),
            qty: "0.25".to_string(),
            order_status: "New".to_string(),
            position_idx: 1,
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("positionIdx=0"));
    }
}
