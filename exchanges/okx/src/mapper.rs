use anyhow::{Context, Result, anyhow};
use chrono::Utc;

use poise_core::track::{Instrument, Venue};
use poise_core::types::Side;
use poise_engine::ports::{AccountSummarySnapshot, ExchangeOrder, OrderStatus, Position};

use crate::instrument::OkxInstrumentMetadata;
use crate::rest::models::{BalanceSnapshot, PendingOrderSnapshot, PositionSnapshot};

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

pub(crate) fn available_balance_from_balance(
    value: &BalanceSnapshot,
    quote_asset: &str,
) -> Result<f64> {
    let detail = value
        .details
        .iter()
        .find(|detail| detail.currency == quote_asset)
        .with_context(|| format!("missing OKX balance detail for quote asset `{quote_asset}`"))?;

    parse_decimal(&format!("details[{quote_asset}].availEq"), &detail.avail_eq)
}

#[cfg(test)]
pub(crate) fn position_from_snapshot(value: PositionSnapshot) -> Result<Position> {
    position_from_snapshot_with_metadata(value, None)
}

pub(crate) fn position_from_snapshot_with_metadata(
    value: PositionSnapshot,
    metadata: Option<&OkxInstrumentMetadata>,
) -> Result<Position> {
    if value.pos_side != "net" {
        return Err(anyhow!(
            "OKX position snapshot requires posSide=net, got {}",
            value.pos_side
        ));
    }
    let qty = native_qty_from_okx_contracts(parse_decimal("pos", &value.pos)?, metadata);
    if qty == 0.0 {
        return Ok(Position {
            instrument: Instrument::new(Venue::Okx, value.inst_id),
            qty: 0.0,
            avg_price: 0.0,
            unrealized_pnl: 0.0,
        });
    }

    Ok(Position {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        qty,
        avg_price: parse_decimal("avgPx", &value.avg_px)?,
        unrealized_pnl: parse_decimal("upl", &value.upl)?,
    })
}

#[cfg(test)]
pub(crate) fn open_order_from_snapshot(value: PendingOrderSnapshot) -> Result<ExchangeOrder> {
    open_order_from_snapshot_with_metadata(value, None)
}

pub(crate) fn open_order_from_snapshot_with_metadata(
    value: PendingOrderSnapshot,
    metadata: Option<&OkxInstrumentMetadata>,
) -> Result<ExchangeOrder> {
    let price = value
        .fill_price
        .as_deref()
        .filter(|fill_price| value.price.is_empty() && !fill_price.is_empty())
        .unwrap_or(&value.price);

    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        order_id: value.order_id,
        client_order_id: value.client_order_id,
        side: side_from_okx(&value.side)?,
        price: parse_decimal("px", price)?,
        qty: native_qty_from_okx_contracts(parse_decimal("sz", &value.size)?, metadata),
        filled_qty: native_qty_from_okx_contracts(
            parse_decimal("accFillSz", &value.acc_fill_sz)?,
            metadata,
        ),
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

pub(crate) fn native_qty_from_okx_contracts(
    okx_contract_qty: f64,
    metadata: Option<&OkxInstrumentMetadata>,
) -> f64 {
    metadata
        .map(|metadata| metadata.native_qty_from_okx_contracts(okx_contract_qty))
        .unwrap_or(okx_contract_qty)
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
    use crate::instrument::{OkxInstrumentMetadata, exchange_info_from_instrument};
    use crate::rest::models::{
        BalanceDetail, BalanceSnapshot, InstrumentInfo, PendingOrderSnapshot, PositionSnapshot,
    };

    #[test]
    fn maps_instrument_info_to_exchange_info() {
        let info = exchange_info_from_instrument(InstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ct_type: Some("linear".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "0.01".to_string(),
            min_sz: "0.01".to_string(),
            ct_val: Some("0.01".to_string()),
            ct_val_ccy: Some("BTC".to_string()),
            settle_ccy: Some("USDT".to_string()),
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
                price_precision: Default::default(),
                quantity_kind: Default::default(),
                contract_notional: None,
                settlement_asset: "USDT".to_string(),
                quantity_step: 0.0001,
                min_qty: 0.0001,
                min_notional: 0.0,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
            }
        );
    }

    #[test]
    fn maps_inverse_instrument_info_to_contract_rules() {
        let info = exchange_info_from_instrument(InstrumentInfo {
            inst_id: "BTC-USD-SWAP".to_string(),
            ct_type: Some("inverse".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "1".to_string(),
            min_sz: "1".to_string(),
            ct_val: Some("100".to_string()),
            ct_val_ccy: Some("USD".to_string()),
            settle_ccy: Some("BTC".to_string()),
        })
        .unwrap();

        assert_eq!(info.instrument, Instrument::new(Venue::Okx, "BTC-USD-SWAP"));
        assert_eq!(
            info.rules.quantity_kind,
            poise_core::types::QuantityKind::InverseContract
        );
        assert_eq!(info.rules.contract_notional, Some(100.0));
        assert_eq!(info.rules.settlement_asset, "BTC");
        assert_eq!(info.rules.quantity_step, 1.0);
        assert_eq!(info.rules.min_qty, 1.0);
    }

    #[test]
    fn rejects_inverse_instrument_without_settlement_metadata() {
        let error = exchange_info_from_instrument(InstrumentInfo {
            inst_id: "BTC-USD-SWAP".to_string(),
            ct_type: Some("inverse".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "1".to_string(),
            min_sz: "1".to_string(),
            ct_val: Some("100".to_string()),
            ct_val_ccy: Some("USD".to_string()),
            settle_ccy: None,
        })
        .unwrap_err();

        assert!(error.to_string().contains("settleCcy"));
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
    fn maps_available_balance_for_quote_asset() {
        let balance = BalanceSnapshot {
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
        };

        assert_eq!(
            available_balance_from_balance(&balance, "USDT").unwrap(),
            9_800.25
        );
        assert_eq!(
            available_balance_from_balance(&balance, "BTC").unwrap(),
            200.0
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
    fn maps_linear_position_contracts_to_base_asset_quantity() {
        let metadata = linear_metadata();
        let position = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                pos: "-30".to_string(),
                avg_px: "65000.5".to_string(),
                upl: "123.45".to_string(),
                pos_side: "net".to_string(),
                lever: "20".to_string(),
            },
            Some(&metadata),
        )
        .unwrap();

        assert_eq!(position.qty, -0.3);
    }

    #[test]
    fn maps_inverse_position_as_contract_quantity() {
        let metadata = inverse_metadata();
        let position = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                pos: "-30".to_string(),
                avg_px: "65000.5".to_string(),
                upl: "0.0012".to_string(),
                pos_side: "net".to_string(),
                lever: "20".to_string(),
            },
            Some(&metadata),
        )
        .unwrap();

        assert_eq!(position.qty, -30.0);
    }

    #[test]
    fn maps_okx_flat_position_snapshot_with_empty_derived_fields() {
        let position = position_from_snapshot(PositionSnapshot {
            inst_id: "MU-USDT-SWAP".to_string(),
            pos: "0".to_string(),
            avg_px: "".to_string(),
            upl: "".to_string(),
            pos_side: "net".to_string(),
            lever: "".to_string(),
        })
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Okx, "MU-USDT-SWAP"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
            }
        );
    }

    #[test]
    fn rejects_tiny_non_zero_position_with_empty_derived_fields() {
        let error = position_from_snapshot(PositionSnapshot {
            inst_id: "MU-USDT-SWAP".to_string(),
            pos: "0.0000000000000001".to_string(),
            avg_px: "".to_string(),
            upl: "".to_string(),
            pos_side: "net".to_string(),
            lever: "".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("avgPx"));
    }

    #[test]
    fn rejects_non_flat_position_with_empty_avg_price() {
        let error = position_from_snapshot(PositionSnapshot {
            inst_id: "MU-USDT-SWAP".to_string(),
            pos: "0.25".to_string(),
            avg_px: "".to_string(),
            upl: "12.3".to_string(),
            pos_side: "net".to_string(),
            lever: "".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("avgPx"));
    }

    #[test]
    fn rejects_non_flat_position_with_empty_unrealized_pnl() {
        let error = position_from_snapshot(PositionSnapshot {
            inst_id: "MU-USDT-SWAP".to_string(),
            pos: "0.25".to_string(),
            avg_px: "650.5".to_string(),
            upl: "".to_string(),
            pos_side: "net".to_string(),
            lever: "".to_string(),
        })
        .unwrap_err();

        assert!(error.to_string().contains("upl"));
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
            fill_price: None,
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
    fn maps_linear_order_contracts_to_base_asset_quantity() {
        let metadata = linear_metadata();
        let order = open_order_from_snapshot_with_metadata(
            PendingOrderSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                order_id: "123".to_string(),
                client_order_id: "client-123".to_string(),
                side: "buy".to_string(),
                price: "65000.1".to_string(),
                fill_price: None,
                size: "30".to_string(),
                acc_fill_sz: "10".to_string(),
                state: "partially_filled".to_string(),
            },
            Some(&metadata),
        )
        .unwrap();

        assert_eq!(order.qty, 0.3);
        assert_eq!(order.filled_qty, 0.1);
    }

    #[test]
    fn maps_inverse_order_as_contract_quantity() {
        let metadata = inverse_metadata();
        let order = open_order_from_snapshot_with_metadata(
            PendingOrderSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                order_id: "123".to_string(),
                client_order_id: "client-123".to_string(),
                side: "sell".to_string(),
                price: "65000.1".to_string(),
                fill_price: None,
                size: "30".to_string(),
                acc_fill_sz: "10".to_string(),
                state: "partially_filled".to_string(),
            },
            Some(&metadata),
        )
        .unwrap();

        assert_eq!(order.qty, 30.0);
        assert_eq!(order.filled_qty, 10.0);
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

    fn linear_metadata() -> OkxInstrumentMetadata {
        OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ct_type: Some("linear".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "0.01".to_string(),
            min_sz: "0.01".to_string(),
            ct_val: Some("0.01".to_string()),
            ct_val_ccy: Some("BTC".to_string()),
            settle_ccy: Some("USDT".to_string()),
        })
        .unwrap()
    }

    fn inverse_metadata() -> OkxInstrumentMetadata {
        OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
            inst_id: "BTC-USD-SWAP".to_string(),
            ct_type: Some("inverse".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "1".to_string(),
            min_sz: "1".to_string(),
            ct_val: Some("100".to_string()),
            ct_val_ccy: Some("USD".to_string()),
            settle_ccy: Some("BTC".to_string()),
        })
        .unwrap()
    }
}
