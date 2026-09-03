use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{TimeZone, Utc};

use poise_application::exchange::TrackPnlRecord;
use poise_application::exchange::{AccountSummarySnapshot, ExchangeOrder, OrderStatus, Position};
use poise_core::instrument::{Amount, Asset, PositionQty};
use poise_core::track::{Instrument, Venue};
use poise_core::types::Side;

use crate::instrument::OkxInstrumentMetadata;
use crate::rest::models::{
    BalanceSnapshot, FundingBillSnapshot, PendingOrderSnapshot, PositionSnapshot, TradeFillSnapshot,
};

const OKX_FUNDING_EXPENSE_SUBTYPE: &str = "173";
const OKX_FUNDING_INCOME_SUBTYPE: &str = "174";

pub(crate) fn account_summary_from_balance(
    value: BalanceSnapshot,
) -> Result<AccountSummarySnapshot> {
    if value.details.is_empty() {
        return Err(anyhow!("missing OKX balance details"));
    }

    let mut available_by_asset = BTreeMap::new();
    for detail in value.details {
        let asset = Asset::try_new(detail.currency).map_err(anyhow::Error::msg)?;
        if let Some(available) = detail
            .avail_bal
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            available_by_asset.insert(
                asset.clone(),
                parse_decimal(&format!("details[{}].availBal", asset.as_str()), available)?,
            );
        }
    }

    let valuation_asset = Asset::try_new("USD").map_err(anyhow::Error::msg)?;
    AccountSummarySnapshot::try_new(
        Amount::new(parse_decimal("totalEq", &value.total_eq)?, valuation_asset)
            .map_err(anyhow::Error::msg)?,
        None,
        available_by_asset,
        None,
        Utc::now(),
    )
    .map_err(anyhow::Error::msg)
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

    let available = detail.avail_bal.as_deref().with_context(|| {
        format!("missing OKX available balance for quote asset `{quote_asset}`")
    })?;
    parse_decimal(&format!("details[{quote_asset}].availBal"), available)
}

pub(crate) fn position_from_snapshot_with_metadata(
    value: PositionSnapshot,
    metadata: &OkxInstrumentMetadata,
) -> Result<Position> {
    if value.pos_side != "net" {
        return Err(anyhow!(
            "OKX position snapshot requires posSide=net, got {}",
            value.pos_side
        ));
    }
    let qty = position_qty_from_okx_contracts(parse_decimal("pos", &value.pos)?, metadata)?;
    let mark_price = parse_optional_decimal("markPx", value.mark_px.as_deref())?;
    if qty.value() == 0.0 {
        return Ok(Position {
            instrument: Instrument::new(Venue::Okx, value.inst_id),
            qty: PositionQty::default(),
            avg_price: 0.0,
            unrealized_pnl: 0.0,
            mark_price,
        });
    }

    Ok(Position {
        instrument: Instrument::new(Venue::Okx, value.inst_id),
        qty,
        avg_price: parse_decimal("avgPx", &value.avg_px)?,
        unrealized_pnl: parse_decimal("upl", &value.upl)?,
        mark_price,
    })
}

pub(crate) fn open_order_from_snapshot_with_metadata(
    value: PendingOrderSnapshot,
    metadata: &OkxInstrumentMetadata,
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
        qty: position_qty_from_okx_contracts(parse_decimal("sz", &value.size)?, metadata)?,
        filled_qty: position_qty_from_okx_contracts(
            parse_decimal("accFillSz", &value.acc_fill_sz)?,
            metadata,
        )?,
        status: order_status_from_okx_state(&value.state)?,
    })
}

pub(crate) fn track_pnl_record_from_trade_fill_with_metadata(
    value: TradeFillSnapshot,
    metadata: &OkxInstrumentMetadata,
) -> Result<TrackPnlRecord> {
    let instrument = Instrument::new(Venue::Okx, value.inst_id);
    let occurred_at = millis_to_utc(&value.ts)?;
    let side = side_from_okx(&value.side)?;
    let fill_price = parse_decimal("fillPx", &value.fill_price)?;
    let fill_size =
        position_qty_from_okx_contracts(parse_decimal("fillSz", &value.fill_size)?, metadata)?;
    let realized_pnl = parse_decimal("fillPnl", &value.fill_pnl)?;
    let raw_fee = parse_optional_decimal("fee", value.fee.as_deref())?;
    let settlement_asset =
        Asset::try_new(metadata.settlement_asset()).map_err(anyhow::Error::msg)?;
    let fee_asset = value
        .fee_currency
        .as_deref()
        .map(str::trim)
        .filter(|asset| !asset.is_empty())
        .map(Asset::try_new)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let trading_fee = raw_fee
        .map(|fee| cash_amount(-fee, fee_asset.unwrap_or_else(|| settlement_asset.clone())))
        .transpose()?;
    let trade_id = non_empty_string(value.trade_id);
    let source_key = trade_id.as_ref().map(|trade_id| {
        format!(
            "okx:orders:{}:{}",
            instrument.symbol.to_lowercase(),
            trade_id
        )
    });

    Ok(TrackPnlRecord::trade(
        instrument,
        occurred_at,
        "okx:fills".to_string(),
        source_key,
        non_empty_string(value.order_id),
        trade_id,
        side,
        fill_price,
        fill_size,
        cash_amount(realized_pnl, settlement_asset)?,
        trading_fee,
    ))
}

pub(crate) fn track_pnl_record_from_funding_bill_with_metadata(
    value: FundingBillSnapshot,
    metadata: &OkxInstrumentMetadata,
) -> Result<TrackPnlRecord> {
    ensure!(
        matches!(
            value.sub_type.as_str(),
            OKX_FUNDING_EXPENSE_SUBTYPE | OKX_FUNDING_INCOME_SUBTYPE
        ),
        "unsupported OKX funding bill subType `{}`",
        value.sub_type
    );
    let instrument = Instrument::new(Venue::Okx, value.inst_id);
    let pnl_asset = metadata.settlement_asset();
    ensure!(
        value.currency == pnl_asset,
        "OKX funding asset `{}` does not match settlement asset `{pnl_asset}`",
        value.currency
    );
    let bill_id = non_empty_string(value.bill_id).context("OKX funding bill missing billId")?;
    let source_key = format!("okx:bills:{}:{}", instrument.symbol.to_lowercase(), bill_id);

    Ok(TrackPnlRecord::funding(
        instrument,
        millis_to_utc(&value.ts)?,
        "okx:bills".to_string(),
        Some(source_key),
        cash_amount(
            parse_decimal("balChg", &value.balance_change)?,
            Asset::try_new(value.currency).map_err(anyhow::Error::msg)?,
        )?,
    ))
}

fn cash_amount(value: f64, asset: Asset) -> Result<Amount> {
    Amount::new(value, asset).map_err(anyhow::Error::msg)
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

pub(crate) fn position_qty_from_okx_contracts(
    okx_contract_qty: f64,
    metadata: &OkxInstrumentMetadata,
) -> Result<PositionQty> {
    metadata.position_qty_from_wire_contracts(okx_contract_qty)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn parse_optional_decimal(field: &str, value: Option<&str>) -> Result<Option<f64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    parse_decimal(field, value).map(Some)
}

fn millis_to_utc(value: &str) -> Result<chrono::DateTime<Utc>> {
    let timestamp_ms = value
        .parse::<i64>()
        .with_context(|| format!("invalid OKX timestamp: {value}"))?;
    Utc.timestamp_millis_opt(timestamp_ms)
        .single()
        .context("invalid OKX timestamp millis")
}

fn non_empty_string(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use poise_application::exchange::{ExchangeOrder, OrderStatus, Position};
    use poise_core::instrument::{Amount, Asset, PositionBasis, PositionQty};
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::Side;

    use super::*;
    use crate::instrument::{OkxInstrumentMetadata, exchange_info_from_instrument};
    use crate::rest::models::{
        BalanceDetail, BalanceSnapshot, InstrumentInfo, PendingOrderSnapshot, PositionSnapshot,
        TradeFillSnapshot,
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

        let spec = info.instrument_spec;
        assert_eq!(
            spec.instrument(),
            &Instrument::new(Venue::Okx, "BTC-USDT-SWAP")
        );
        assert_eq!(spec.position_basis(), &PositionBasis::BaseAsset);
        assert_eq!(spec.price_tick(), 0.1);
        assert_eq!(spec.position_qty_step(), PositionQty::new(0.0001).unwrap());
        assert_eq!(spec.min_position_qty(), PositionQty::new(0.0001).unwrap());
        assert_eq!(
            spec.min_notional(),
            &Amount::new(0.0, Asset::try_new("USDT").unwrap()).unwrap()
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

        let spec = info.instrument_spec;
        assert_eq!(
            spec.instrument(),
            &Instrument::new(Venue::Okx, "BTC-USD-SWAP")
        );
        assert_eq!(
            spec.position_basis(),
            &PositionBasis::Contracts {
                contract_value: Amount::new(100.0, Asset::try_new("USD").unwrap()).unwrap(),
            }
        );
        assert_eq!(spec.settlement_asset().as_str(), "BTC");
        assert_eq!(spec.position_qty_step(), PositionQty::new(1.0).unwrap());
        assert_eq!(spec.min_position_qty(), PositionQty::new(1.0).unwrap());
    }

    #[test]
    fn maps_inverse_trade_fill_to_contract_qty_and_settlement_pnl_asset() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
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

        let record = track_pnl_record_from_trade_fill_with_metadata(
            TradeFillSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                order_id: "order-1".to_string(),
                trade_id: "trade-1".to_string(),
                side: "sell".to_string(),
                fill_price: "62147.4".to_string(),
                fill_size: "10".to_string(),
                fill_pnl: "0.000000154839457".to_string(),
                fee: Some("-0.000000032".to_string()),
                fee_currency: Some("BTC".to_string()),
                ts: "1781099765546".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(
            record.instrument,
            Instrument::new(Venue::Okx, "BTC-USD-SWAP")
        );
        assert_eq!(record.realized_pnl().unwrap().asset().as_str(), "BTC");
        assert_eq!(record.source, "okx:fills");
        assert_eq!(
            record.source_key.as_deref(),
            Some("okx:orders:btc-usd-swap:trade-1")
        );
        assert_eq!(record.order_id.as_deref(), Some("order-1"));
        assert_eq!(record.trade_id.as_deref(), Some("trade-1"));
        assert_eq!(record.side, Some(Side::Sell));
        assert_eq!(record.price, Some(62_147.4));
        assert_eq!(record.qty.map(PositionQty::value), Some(10.0));
        assert_eq!(record.realized_pnl().unwrap().value(), 0.000000154839457);
        assert_eq!(record.trading_fee().unwrap().value(), 0.000000032);
        assert_eq!(record.occurred_at.timestamp_millis(), 1_781_099_765_546);
    }

    #[test]
    fn maps_linear_trade_fill_to_base_asset_qty() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
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

        let record = track_pnl_record_from_trade_fill_with_metadata(
            TradeFillSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                order_id: "order-1".to_string(),
                trade_id: "trade-1".to_string(),
                side: "buy".to_string(),
                fill_price: "64000".to_string(),
                fill_size: "3".to_string(),
                fill_pnl: "12.34".to_string(),
                fee: Some("-0.5".to_string()),
                fee_currency: Some("USDT".to_string()),
                ts: "1781099765546".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(record.realized_pnl().unwrap().asset().as_str(), "USDT");
        assert_eq!(record.qty.map(PositionQty::value), Some(0.03));
        assert_eq!(record.realized_pnl().unwrap().value(), 12.34);
        assert_eq!(record.trading_fee().unwrap().value(), 0.5);
    }

    #[test]
    fn trade_fill_preserves_non_settlement_fee_asset() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
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

        let record = track_pnl_record_from_trade_fill_with_metadata(
            TradeFillSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                order_id: "order-1".to_string(),
                trade_id: "trade-1".to_string(),
                side: "sell".to_string(),
                fill_price: "62147.4".to_string(),
                fill_size: "10".to_string(),
                fill_pnl: "0".to_string(),
                fee: Some("-0.000000032".to_string()),
                fee_currency: Some("USD".to_string()),
                ts: "1781099765546".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(record.realized_pnl().unwrap().asset().as_str(), "BTC");
        assert_eq!(record.trading_fee().unwrap().asset().as_str(), "USD");
    }

    #[test]
    fn maps_funding_bill_to_track_pnl_record() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
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

        let record = track_pnl_record_from_funding_bill_with_metadata(
            FundingBillSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                bill_id: "bill-1".to_string(),
                sub_type: "174".to_string(),
                balance_change: "0.00025".to_string(),
                currency: "BTC".to_string(),
                ts: "1781145600000".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(
            record.kind(),
            poise_application::exchange::TrackPnlRecordKind::Funding
        );
        assert_eq!(record.source, "okx:bills");
        assert_eq!(
            record.source_key.as_deref(),
            Some("okx:bills:btc-usd-swap:bill-1")
        );
        assert_eq!(record.funding_amount().unwrap().asset().as_str(), "BTC");
        assert_eq!(record.funding_amount().unwrap().value(), 0.00025);
        assert!(record.realized_pnl().is_none());
        assert!(record.trading_fee().is_none());
    }

    #[test]
    fn funding_bill_rejects_asset_mismatch() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
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

        let error = track_pnl_record_from_funding_bill_with_metadata(
            FundingBillSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                bill_id: "bill-1".to_string(),
                sub_type: "173".to_string(),
                balance_change: "-0.00025".to_string(),
                currency: "USDT".to_string(),
                ts: "1781145600000".to_string(),
            },
            &metadata,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("USDT"));
        assert!(message.contains("BTC"));
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
                    avail_bal: Some("9800.25".to_string()),
                },
                BalanceDetail {
                    currency: "BTC".to_string(),
                    avail_bal: Some("0.2".to_string()),
                },
            ],
        })
        .unwrap();

        assert_eq!(summary.valuation_asset().as_str(), "USD");
        assert_eq!(summary.equity().value(), 12_500.5);
        assert_eq!(summary.available(), None);
        assert_eq!(summary.unrealized_pnl(), None);
        assert_eq!(summary.available_for_asset("BTC"), Some(0.2));
        assert_eq!(summary.available_for_asset("USDT"), Some(9_800.25));
    }

    #[test]
    fn maps_available_balance_for_quote_asset() {
        let balance = BalanceSnapshot {
            total_eq: "12500.5".to_string(),
            details: vec![
                BalanceDetail {
                    currency: "USDT".to_string(),
                    avail_bal: Some("9800.25".to_string()),
                },
                BalanceDetail {
                    currency: "BTC".to_string(),
                    avail_bal: Some("0.2".to_string()),
                },
            ],
        };

        assert_eq!(
            available_balance_from_balance(&balance, "USDT").unwrap(),
            9_800.25
        );
        assert_eq!(
            available_balance_from_balance(&balance, "BTC").unwrap(),
            0.2
        );
    }

    #[test]
    fn maps_net_position_snapshot_to_signed_position() {
        let metadata = linear_metadata();
        let position = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                pos: "-25".to_string(),
                avg_px: "65000.5".to_string(),
                mark_px: Some("65100.5".to_string()),
                upl: "123.45".to_string(),
                pos_side: "net".to_string(),
                lever: "20".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                qty: PositionQty::new(-0.25).unwrap(),
                avg_price: 65000.5,
                unrealized_pnl: 123.45,
                mark_price: Some(65100.5),
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
                mark_px: None,
                upl: "123.45".to_string(),
                pos_side: "net".to_string(),
                lever: "20".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(position.qty.value(), -0.3);
    }

    #[test]
    fn maps_inverse_position_as_contract_quantity() {
        let metadata = inverse_metadata();
        let position = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "BTC-USD-SWAP".to_string(),
                pos: "-30".to_string(),
                avg_px: "65000.5".to_string(),
                mark_px: Some("65100.5".to_string()),
                upl: "0.0012".to_string(),
                pos_side: "net".to_string(),
                lever: "20".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(position.qty.value(), -30.0);
    }

    #[test]
    fn maps_okx_flat_position_snapshot_with_empty_derived_fields() {
        let metadata = linear_metadata_for("MU-USDT-SWAP", "MU");
        let position = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "MU-USDT-SWAP".to_string(),
                pos: "0".to_string(),
                avg_px: "".to_string(),
                mark_px: None,
                upl: "".to_string(),
                pos_side: "net".to_string(),
                lever: "".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Okx, "MU-USDT-SWAP"),
                qty: PositionQty::default(),
                avg_price: 0.0,
                unrealized_pnl: 0.0,
                mark_price: None,
            }
        );
    }

    #[test]
    fn rejects_tiny_non_zero_position_with_empty_derived_fields() {
        let metadata = linear_metadata_for("MU-USDT-SWAP", "MU");
        let error = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "MU-USDT-SWAP".to_string(),
                pos: "0.0000000000000001".to_string(),
                avg_px: "".to_string(),
                mark_px: None,
                upl: "".to_string(),
                pos_side: "net".to_string(),
                lever: "".to_string(),
            },
            &metadata,
        )
        .unwrap_err();

        assert!(error.to_string().contains("avgPx"));
    }

    #[test]
    fn rejects_non_flat_position_with_empty_avg_price() {
        let metadata = linear_metadata_for("MU-USDT-SWAP", "MU");
        let error = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "MU-USDT-SWAP".to_string(),
                pos: "25".to_string(),
                avg_px: "".to_string(),
                mark_px: None,
                upl: "12.3".to_string(),
                pos_side: "net".to_string(),
                lever: "".to_string(),
            },
            &metadata,
        )
        .unwrap_err();

        assert!(error.to_string().contains("avgPx"));
    }

    #[test]
    fn rejects_non_flat_position_with_empty_unrealized_pnl() {
        let metadata = linear_metadata_for("MU-USDT-SWAP", "MU");
        let error = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "MU-USDT-SWAP".to_string(),
                pos: "25".to_string(),
                avg_px: "650.5".to_string(),
                mark_px: None,
                upl: "".to_string(),
                pos_side: "net".to_string(),
                lever: "".to_string(),
            },
            &metadata,
        )
        .unwrap_err();

        assert!(error.to_string().contains("upl"));
    }

    #[test]
    fn rejects_long_short_position_mode() {
        let metadata = linear_metadata();
        let error = position_from_snapshot_with_metadata(
            PositionSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                pos: "25".to_string(),
                avg_px: "65000.5".to_string(),
                mark_px: None,
                upl: "123.45".to_string(),
                pos_side: "long".to_string(),
                lever: "20".to_string(),
            },
            &metadata,
        )
        .unwrap_err();

        assert!(error.to_string().contains("posSide=net"));
    }

    #[test]
    fn maps_pending_order_snapshot_to_exchange_order() {
        let metadata = linear_metadata();
        let order = open_order_from_snapshot_with_metadata(
            PendingOrderSnapshot {
                inst_id: "BTC-USDT-SWAP".to_string(),
                order_id: "123".to_string(),
                client_order_id: "client-123".to_string(),
                side: "buy".to_string(),
                price: "65000.1".to_string(),
                fill_price: None,
                size: "20".to_string(),
                acc_fill_sz: "5".to_string(),
                state: "partially_filled".to_string(),
            },
            &metadata,
        )
        .unwrap();

        assert_eq!(
            order,
            ExchangeOrder {
                instrument: Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
                order_id: "123".to_string(),
                client_order_id: "client-123".to_string(),
                side: Side::Buy,
                price: 65000.1,
                qty: PositionQty::new(0.2).unwrap(),
                filled_qty: PositionQty::new(0.05).unwrap(),
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
            &metadata,
        )
        .unwrap();

        assert_eq!(order.qty.value(), 0.3);
        assert_eq!(order.filled_qty.value(), 0.1);
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
            &metadata,
        )
        .unwrap();

        assert_eq!(order.qty.value(), 30.0);
        assert_eq!(order.filled_qty.value(), 10.0);
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
        linear_metadata_for("BTC-USDT-SWAP", "BTC")
    }

    fn linear_metadata_for(inst_id: &str, base_asset: &str) -> OkxInstrumentMetadata {
        OkxInstrumentMetadata::from_instrument_info(&InstrumentInfo {
            inst_id: inst_id.to_string(),
            ct_type: Some("linear".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "0.01".to_string(),
            min_sz: "0.01".to_string(),
            ct_val: Some("0.01".to_string()),
            ct_val_ccy: Some(base_asset.to_string()),
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
