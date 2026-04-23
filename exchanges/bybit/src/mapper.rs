use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};

use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderStatus, Position,
};
use poise_engine::track::{Instrument, Venue};

use crate::protocol::BybitOrderStatus;
use crate::rest::models::{
    CreateOrderResult, InstrumentInfoResult, OpenOrderSnapshot, PositionSnapshot, ServerTimeResult,
    UnifiedWalletBalance, WalletBalanceResult,
};

pub(crate) struct BybitActiveOrder {
    pub symbol: String,
    pub order_id: String,
    pub client_order_id: Option<String>,
    pub side: poise_core::types::Side,
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    pub order_status: BybitOrderStatus,
    pub position_idx: i64,
}

pub(crate) fn build_account_capacity_snapshot(
    wallet_balance: &WalletBalanceResult,
    leverage: f64,
) -> Result<AccountCapacitySnapshot> {
    let balance = first_wallet_balance(wallet_balance)?;
    let available = required_value("totalAvailableBalance", balance.total_available_balance)?;
    Ok(AccountCapacitySnapshot {
        max_increase_notional: available * leverage,
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
                price_tick: required_value("priceFilter.tickSize", info.price_filter.tick_size)?,
                quantity_step: required_value(
                    "lotSizeFilter.qtyStep",
                    info.lot_size_filter.qty_step,
                )?,
                min_qty: required_value(
                    "lotSizeFilter.minOrderQty",
                    info.lot_size_filter.min_order_qty,
                )?,
                min_notional: required_value(
                    "lotSizeFilter.minNotionalValue",
                    info.lot_size_filter.min_notional_value,
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
            equity: required_value("totalEquity", balance.total_equity)?,
            available: required_value("totalAvailableBalance", balance.total_available_balance)?,
            unrealized_pnl: required_value("totalPerpUPL", balance.total_perp_upl)?,
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
            filled_qty: 0.0,
            status: OrderStatus::Submitting,
        })
    }
}

impl TryFrom<PositionSnapshot> for Position {
    type Error = anyhow::Error;

    fn try_from(value: PositionSnapshot) -> Result<Self, Self::Error> {
        build_bybit_position(
            value.symbol,
            value.side,
            value.size,
            value.avg_price,
            value.unrealised_pnl,
            value.position_idx,
        )
    }
}

impl TryFrom<OpenOrderSnapshot> for ExchangeOrder {
    type Error = anyhow::Error;

    fn try_from(value: OpenOrderSnapshot) -> Result<Self, Self::Error> {
        build_bybit_open_order(BybitActiveOrder {
            symbol: value.symbol,
            order_id: value.order_id,
            client_order_id: value.order_link_id,
            side: value.side,
            price: value.price,
            qty: value.qty,
            filled_qty: value.cum_exec_qty.unwrap_or(0.0),
            order_status: value.order_status,
            position_idx: value.position_idx,
        })
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

fn required_value(field: &str, value: Option<f64>) -> Result<f64> {
    value.ok_or_else(|| anyhow!("missing required {field}"))
}

pub(crate) fn side_to_bybit(side: poise_core::types::Side) -> &'static str {
    match side {
        poise_core::types::Side::Buy => "Buy",
        poise_core::types::Side::Sell => "Sell",
    }
}

pub(crate) fn should_track_bybit_order(
    order_status: BybitOrderStatus,
    stop_order_type: Option<&str>,
) -> bool {
    let stop_order_type = stop_order_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("UNKNOWN");
    if !stop_order_type.eq_ignore_ascii_case("UNKNOWN") {
        return false;
    }

    order_status.is_trackable()
}

pub(crate) fn build_bybit_position(
    symbol: String,
    side: Option<poise_core::types::Side>,
    size: f64,
    avg_price: Option<f64>,
    unrealised_pnl: Option<f64>,
    position_idx: i64,
) -> Result<Position> {
    if position_idx != 0 {
        return Err(anyhow!(
            "Bybit one-way position snapshot requires positionIdx=0, got {position_idx}"
        ));
    }

    let signed_qty = match side {
        Some(poise_core::types::Side::Buy) => size,
        Some(poise_core::types::Side::Sell) => -size,
        None if size == 0.0 => 0.0,
        None => {
            return Err(anyhow!(
                "Bybit position side is empty for non-flat size {size}"
            ));
        }
    };
    let allow_blank_numeric = size == 0.0;

    Ok(Position {
        instrument: Instrument::new(Venue::Bybit, symbol),
        qty: signed_qty,
        avg_price: value_or_zero("avgPrice", avg_price, allow_blank_numeric)?,
        unrealized_pnl: value_or_zero("unrealisedPnl", unrealised_pnl, allow_blank_numeric)?,
    })
}

pub(crate) fn build_bybit_open_order(order: BybitActiveOrder) -> Result<ExchangeOrder> {
    let BybitActiveOrder {
        symbol,
        order_id,
        client_order_id,
        side,
        price,
        qty,
        filled_qty,
        order_status,
        position_idx,
    } = order;

    if position_idx != 0 {
        return Err(anyhow!(
            "Bybit one-way order snapshot requires positionIdx=0, got {position_idx}"
        ));
    }

    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Bybit, symbol),
        order_id,
        client_order_id: client_order_id.unwrap_or_default(),
        side,
        price,
        qty,
        filled_qty,
        realized_pnl: 0.0,
        status: order_status
            .into_order_status()
            .ok_or_else(|| anyhow!("unsupported Bybit order status: {order_status:?}"))?,
    })
}

fn value_or_zero(field: &str, value: Option<f64>, allow_blank_zero: bool) -> Result<f64> {
    match (value, allow_blank_zero) {
        (Some(value), _) => Ok(value),
        (None, true) => Ok(0.0),
        (None, false) => Err(anyhow!("missing required {field}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BybitOrderStatus;
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
                tick_size: Some(0.10),
            },
            lot_size_filter: LotSizeFilter {
                qty_step: Some(0.001),
                min_order_qty: Some(0.001),
                min_notional_value: Some(5.0),
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
                total_equity: Some(125.5),
                total_available_balance: Some(100.25),
                total_perp_upl: Some(-2.75),
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
                total_available_balance: Some(100.25),
                total_perp_upl: Some(-2.75),
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
                total_equity: Some(125.5),
                total_available_balance: Some(100.25),
                total_perp_upl: Some(-2.75),
            }],
        };

        let error = balances
            .into_account_summary_snapshot()
            .unwrap_err()
            .to_string();

        assert!(error.contains("accountType=UNIFIED"));
    }

    #[test]
    fn builds_account_capacity_snapshot_from_available_balance_and_leverage() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: Some(125.5),
                total_available_balance: Some(100.25),
                total_perp_upl: Some(-2.75),
            }],
        };

        let snapshot = build_account_capacity_snapshot(&balances, 10.0).unwrap();

        assert_eq!(snapshot.max_increase_notional, 1002.5);
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
                filled_qty: 0.0,
                status: OrderStatus::Submitting,
            }
        );
    }

    #[test]
    fn converts_one_way_position_snapshot_into_position() {
        let position = Position::try_from(PositionSnapshot {
            symbol: "BTCUSDT".to_string(),
            side: Some(Side::Sell),
            size: 0.25,
            leverage: None,
            avg_price: Some(65000.5),
            unrealised_pnl: Some(-12.5),
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
    fn converts_flat_position_snapshot_with_empty_side_into_zero_position() {
        let position = Position::try_from(PositionSnapshot {
            symbol: "BTCUSDT".to_string(),
            side: None,
            size: 0.0,
            leverage: None,
            avg_price: None,
            unrealised_pnl: None,
            position_idx: 0,
        })
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Bybit, "BTCUSDT"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            }
        );
    }

    #[test]
    fn rejects_non_one_way_position_snapshot() {
        let error = Position::try_from(PositionSnapshot {
            symbol: "BTCUSDT".to_string(),
            side: Some(Side::Buy),
            size: 0.25,
            leverage: None,
            avg_price: Some(65000.5),
            unrealised_pnl: Some(-12.5),
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
            side: Side::Buy,
            price: 65000.5,
            qty: 0.25,
            cum_exec_qty: Some(0.1),
            order_status: BybitOrderStatus::PartiallyFilled,
            stop_order_type: None,
            position_idx: 0,
        })
        .unwrap();

        assert_eq!(order.instrument, Instrument::new(Venue::Bybit, "BTCUSDT"));
        assert_eq!(order.client_order_id, "client-1");
        assert_eq!(order.side, Side::Buy);
        assert_eq!(order.price, 65000.5);
        assert_eq!(order.qty, 0.25);
        assert_eq!(order.filled_qty, 0.1);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn rejects_non_one_way_open_order_snapshot() {
        let error = ExchangeOrder::try_from(OpenOrderSnapshot {
            symbol: "BTCUSDT".to_string(),
            order_id: "12345".to_string(),
            order_link_id: Some("client-1".to_string()),
            side: Side::Buy,
            price: 65000.5,
            qty: 0.25,
            cum_exec_qty: None,
            order_status: BybitOrderStatus::New,
            stop_order_type: None,
            position_idx: 1,
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("positionIdx=0"));
    }

    #[test]
    fn ignores_conditional_order_kinds_and_statuses() {
        assert!(!should_track_bybit_order(
            BybitOrderStatus::Untriggered,
            Some("Stop")
        ));
        assert!(!should_track_bybit_order(
            BybitOrderStatus::New,
            Some("Stop")
        ));
        assert!(should_track_bybit_order(
            BybitOrderStatus::New,
            Some("UNKNOWN")
        ));
        assert!(should_track_bybit_order(BybitOrderStatus::Filled, None));
    }
}
