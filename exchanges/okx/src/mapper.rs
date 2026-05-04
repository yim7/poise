use anyhow::{Context, Result, anyhow};
use chrono::Utc;

use poise_core::track::{Instrument, Venue};
use poise_core::types::Side;
use poise_engine::ports::{
    AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderStatus, Position,
};

use crate::rest::models::{
    BalanceSnapshot, InstrumentInfo, PendingOrderSnapshot, PositionSnapshot,
};

pub(crate) fn exchange_info_from_instrument(value: InstrumentInfo) -> Result<ExchangeInfo> {
    if let Some(contract_value) = value.ct_val.as_deref() {
        let _ = parse_decimal("ctVal", contract_value)?;
    }

    Ok(ExchangeInfo {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        rules: poise_core::types::ExchangeRules {
            price_tick: parse_decimal("tickSz", &value.tick_sz)?,
            quantity_step: parse_decimal("lotSz", &value.lot_sz)?,
            min_qty: parse_decimal("minSz", &value.min_sz)?,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        },
    })
}

pub(crate) fn account_summary_from_balance(
    value: BalanceSnapshot,
) -> Result<AccountSummarySnapshot> {
    if value.details.is_empty() {
        return Err(anyhow!("missing OKX balance details"));
    }

    let mut available = 0.0;
    let mut unrealized_pnl = 0.0;
    for detail in value.details {
        let currency = detail.currency;
        available += parse_decimal(&format!("details[{currency}].availEq"), &detail.avail_eq)?;
        unrealized_pnl += parse_decimal(&format!("details[{currency}].upl"), &detail.upl)?;
    }

    Ok(AccountSummarySnapshot {
        equity: parse_decimal("totalEq", &value.total_eq)?,
        available,
        unrealized_pnl,
        observed_at: Utc::now(),
    })
}

pub(crate) fn position_from_snapshot(value: PositionSnapshot) -> Result<Position> {
    if value.pos_side != "net" {
        return Err(anyhow!(
            "OKX position snapshot requires posSide=net, got {}",
            value.pos_side
        ));
    }
    let _leverage = parse_decimal("lever", &value.lever)?;

    Ok(Position {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        qty: parse_decimal("pos", &value.pos)?,
        avg_price: parse_decimal("avgPx", &value.avg_px)?,
        unrealized_pnl: parse_decimal("upl", &value.upl)?,
    })
}

pub(crate) fn open_order_from_snapshot(value: PendingOrderSnapshot) -> Result<ExchangeOrder> {
    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        order_id: value.order_id,
        client_order_id: value.client_order_id,
        side: side_from_okx(&value.side)?,
        price: parse_decimal("px", &value.price)?,
        qty: parse_decimal("sz", &value.size)?,
        filled_qty: parse_decimal("accFillSz", &value.acc_fill_sz)?,
        status: order_status_from_okx_state(&value.state)?,
    })
}

pub(crate) fn order_status_from_okx_state(value: &str) -> Result<OrderStatus> {
    match value {
        "live" => Ok(OrderStatus::New),
        "partially_filled" => Ok(OrderStatus::PartiallyFilled),
        "filled" => Ok(OrderStatus::Filled),
        "canceled" | "mmp_canceled" => Ok(OrderStatus::Canceled),
        other => Err(anyhow!("unsupported OKX order state: {other}")),
    }
}

fn side_from_okx(value: &str) -> Result<Side> {
    match value {
        "buy" => Ok(Side::Buy),
        "sell" => Ok(Side::Sell),
        other => Err(anyhow!("unsupported OKX side: {other}")),
    }
}

pub(crate) fn side_to_okx(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

#[cfg(test)]
mod tests {
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::{ExchangeRules, Side};
    use poise_engine::ports::{AccountSummarySnapshot, ExchangeOrder, OrderStatus, Position};

    use super::*;
    use crate::rest::models::{
        BalanceDetail, BalanceSnapshot, InstrumentInfo, PendingOrderSnapshot, PositionSnapshot,
    };

    #[test]
    fn maps_instrument_info_to_exchange_info() {
        let info = exchange_info_from_instrument(InstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            tick_sz: "0.1".to_string(),
            lot_sz: "0.01".to_string(),
            min_sz: "0.01".to_string(),
            ct_val: Some("0.01".to_string()),
        })
        .unwrap();

        assert_eq!(
            info.instrument,
            Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
        );
        assert_eq!(
            info.rules,
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.01,
                min_qty: 0.01,
                min_notional: 0.0,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
            }
        );
    }

    #[test]
    fn maps_balance_snapshot_to_account_summary() {
        let summary = account_summary_from_balance(BalanceSnapshot {
            total_eq: "12500.5".to_string(),
            details: vec![
                BalanceDetail {
                    currency: "USDT".to_string(),
                    avail_eq: "9800.25".to_string(),
                    upl: "-120.75".to_string(),
                },
                BalanceDetail {
                    currency: "BTC".to_string(),
                    avail_eq: "200.0".to_string(),
                    upl: "10.0".to_string(),
                },
            ],
        })
        .unwrap();

        assert_eq!(
            summary,
            AccountSummarySnapshot {
                equity: 12_500.5,
                available: 10_000.25,
                unrealized_pnl: -110.75,
                observed_at: summary.observed_at,
            }
        );
    }

    #[test]
    fn maps_net_position_snapshot_to_signed_position() {
        let position = position_from_snapshot(PositionSnapshot {
            inst_id: "BTC-USDT-SWAP".to_string(),
            pos: "-0.25".to_string(),
            avg_px: "65000.5".to_string(),
            upl: "123.45".to_string(),
            pos_side: "net".to_string(),
            lever: "20".to_string(),
        })
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                qty: -0.25,
                avg_price: 65000.5,
                unrealized_pnl: 123.45,
            }
        );
    }

    #[test]
    fn rejects_long_short_position_mode() {
        let error = position_from_snapshot(PositionSnapshot {
            inst_id: "BTC-USDT-SWAP".to_string(),
            pos: "0.25".to_string(),
            avg_px: "65000.5".to_string(),
            upl: "123.45".to_string(),
            pos_side: "long".to_string(),
            lever: "20".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("posSide=net"));
    }

    #[test]
    fn maps_pending_order_snapshot_to_exchange_order() {
        let order = open_order_from_snapshot(PendingOrderSnapshot {
            inst_id: "BTC-USDT-SWAP".to_string(),
            order_id: "123".to_string(),
            client_order_id: "client-123".to_string(),
            side: "buy".to_string(),
            price: "65000.1".to_string(),
            size: "0.2".to_string(),
            acc_fill_sz: "0.05".to_string(),
            state: "partially_filled".to_string(),
        })
        .unwrap();

        assert_eq!(
            order,
            ExchangeOrder {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                order_id: "123".to_string(),
                client_order_id: "client-123".to_string(),
                side: Side::Buy,
                price: 65000.1,
                qty: 0.2,
                filled_qty: 0.05,
                status: OrderStatus::PartiallyFilled,
            }
        );
    }

    #[test]
    fn maps_okx_order_states_and_sides() {
        assert_eq!(
            order_status_from_okx_state("live").unwrap(),
            OrderStatus::New
        );
        assert_eq!(
            order_status_from_okx_state("partially_filled").unwrap(),
            OrderStatus::PartiallyFilled
        );
        assert_eq!(
            order_status_from_okx_state("filled").unwrap(),
            OrderStatus::Filled
        );
        assert_eq!(
            order_status_from_okx_state("canceled").unwrap(),
            OrderStatus::Canceled
        );
        assert_eq!(
            order_status_from_okx_state("mmp_canceled").unwrap(),
            OrderStatus::Canceled
        );
        assert_eq!(side_to_okx(Side::Buy), "buy");
        assert_eq!(side_to_okx(Side::Sell), "sell");
    }
}
