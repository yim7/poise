use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use poise_core::instrument::{Amount, Asset, InstrumentSpec};
use poise_core::track::{Instrument, Venue};
use poise_core::types::Side;
use poise_engine::ledger::TrackPnlRecord;
use poise_engine::ports::{
    AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderStatus, Position,
};

use crate::rest::models::{
    ClearinghouseStateResponse, MetaResponse, OpenOrderResponse, UserFillResponse,
    UserFundingResponse, WsUserFundingResponse,
};
use crate::rules::{perp_price_precision, representative_perp_price_tick};
const MIN_NOTIONAL_USD: f64 = 10.0;

pub(crate) fn build_exchange_info(meta: &MetaResponse, symbol: &str) -> Result<ExchangeInfo> {
    let asset = meta
        .universe
        .iter()
        .find(|asset| asset.name == symbol)
        .ok_or_else(|| anyhow!("missing Hyperliquid asset `{symbol}`"))?;
    let quantity_step = decimal_step(asset.sz_decimals as i32);

    let base_asset = Asset::try_new(symbol).map_err(anyhow::Error::msg)?;
    let quote_asset = Asset::try_new("USDC").map_err(anyhow::Error::msg)?;
    Ok(ExchangeInfo {
        instrument_spec: InstrumentSpec::try_linear(
            Instrument::new(Venue::Hyperliquid, symbol),
            base_asset,
            quote_asset.clone(),
            quote_asset.clone(),
            perp_price_precision(asset.sz_decimals),
            representative_perp_price_tick(asset.sz_decimals),
            quantity_step,
            quantity_step,
            Amount::new(MIN_NOTIONAL_USD, quote_asset).map_err(anyhow::Error::msg)?,
        )
        .map_err(anyhow::Error::msg)?,
    })
}

pub(crate) fn account_summary_from_state(
    state: &ClearinghouseStateResponse,
) -> Result<AccountSummarySnapshot> {
    Ok(AccountSummarySnapshot {
        equity: parse_decimal(
            "marginSummary.accountValue",
            &state.margin_summary.account_value,
        )?,
        available: parse_decimal("withdrawable", &state.withdrawable)?,
        available_by_asset: Default::default(),
        unrealized_pnl: state
            .asset_positions
            .iter()
            .map(|asset| parse_decimal("position.unrealizedPnl", &asset.position.unrealized_pnl))
            .sum::<Result<f64>>()?,
        observed_at: Utc::now(),
    })
}

pub(crate) fn position_from_state(
    state: &ClearinghouseStateResponse,
    symbol: &str,
) -> Result<Position> {
    let Some(asset_position) = state
        .asset_positions
        .iter()
        .find(|asset| asset.position.coin == symbol)
    else {
        return Ok(Position {
            instrument: Instrument::new(Venue::Hyperliquid, symbol),
            qty: 0.0,
            avg_price: 0.0,
            unrealized_pnl: 0.0,
            mark_price: None,
        });
    };
    let position = &asset_position.position;
    Ok(Position {
        instrument: Instrument::new(Venue::Hyperliquid, &position.coin),
        qty: parse_decimal("position.szi", &position.szi)?,
        avg_price: position
            .entry_px
            .as_deref()
            .map(|entry_px| parse_decimal("position.entryPx", entry_px))
            .transpose()?
            .unwrap_or(0.0),
        unrealized_pnl: parse_decimal("position.unrealizedPnl", &position.unrealized_pnl)?,
        mark_price: None,
    })
}

pub(crate) fn open_order_from_response(value: OpenOrderResponse) -> Result<ExchangeOrder> {
    let order_id = value.oid.to_string();
    Ok(ExchangeOrder {
        instrument: Instrument::new(Venue::Hyperliquid, value.coin),
        order_id: order_id.clone(),
        client_order_id: value.cloid.unwrap_or(order_id),
        side: parse_side(&value.side)?,
        price: parse_decimal("limitPx", &value.limit_px)?,
        qty: parse_decimal("sz", &value.sz)?,
        filled_qty: 0.0,
        status: OrderStatus::New,
    })
}

pub(crate) fn track_pnl_record_from_fill(value: &UserFillResponse) -> Result<TrackPnlRecord> {
    ensure!(
        value.fee_token.trim() == "USDC",
        "unsupported Hyperliquid fill feeToken `{}`; expected `USDC`",
        value.fee_token
    );
    ensure_non_empty("fill.hash", &value.hash)?;
    let occurred_at = millis_to_utc(value.time)?;
    let trade_id = value.tid.to_string();
    Ok(TrackPnlRecord::trade(
        Instrument::new(Venue::Hyperliquid, &value.coin),
        occurred_at,
        "hyperliquid:fill".to_string(),
        Some(format!("hyperliquid:fill:{}:{trade_id}", value.coin)),
        Some(value.oid.to_string()),
        Some(trade_id),
        parse_side(&value.side)?,
        parse_finite_decimal("fill.px", &value.px)?,
        parse_finite_decimal("fill.sz", &value.sz)?,
        parse_finite_decimal("fill.closedPnl", &value.closed_pnl)?,
        parse_finite_decimal("fill.fee", &value.fee)?,
        "USDC",
    ))
}

pub(crate) fn track_pnl_record_from_funding(value: &UserFundingResponse) -> Result<TrackPnlRecord> {
    ensure!(
        value.delta.kind == "funding",
        "unsupported Hyperliquid funding delta type `{}`",
        value.delta.kind
    );
    ensure_non_empty("funding.hash", &value.hash)?;
    funding_record(&value.delta.coin, value.time, &value.delta.usdc)
}

pub(crate) fn track_pnl_record_from_ws_funding(
    value: &WsUserFundingResponse,
) -> Result<TrackPnlRecord> {
    funding_record(&value.coin, value.time, &value.usdc)
}

fn funding_record(coin: &str, time: i64, usdc: &str) -> Result<TrackPnlRecord> {
    Ok(TrackPnlRecord::funding(
        Instrument::new(Venue::Hyperliquid, coin),
        millis_to_utc(time)?,
        "hyperliquid:funding".to_string(),
        Some(format!("hyperliquid:funding:{coin}:{time}")),
        parse_finite_decimal("funding.usdc", usdc)?,
        "USDC",
    ))
}

fn decimal_step(decimals: i32) -> f64 {
    10_f64.powi(-decimals)
}

fn parse_side(value: &str) -> Result<Side> {
    match value {
        "B" => Ok(Side::Buy),
        "A" => Ok(Side::Sell),
        other => Err(anyhow!("unsupported Hyperliquid side: {other}")),
    }
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .trim()
        .parse::<f64>()
        .with_context(|| format!("invalid Hyperliquid decimal `{field}`: {value}"))
}

fn parse_finite_decimal(field: &str, value: &str) -> Result<f64> {
    let parsed = parse_decimal(field, value)?;
    ensure!(
        parsed.is_finite(),
        "invalid Hyperliquid decimal `{field}`: {value} is not finite"
    );
    Ok(parsed)
}

fn millis_to_utc(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value)
        .ok_or_else(|| anyhow!("invalid Hyperliquid timestamp: {value}"))
}

fn ensure_non_empty<'a>(field: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    ensure!(!value.is_empty(), "missing Hyperliquid `{field}`");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::{PricePrecision, Side};
    use poise_engine::ports::{ExchangeOrder, OrderStatus, Position};

    use crate::rest::models::{
        AssetPosition, ClearinghouseStateResponse, FundingDeltaResponse, MarginSummary,
        MetaResponse, OpenOrderResponse, PerpAssetMeta, PositionData, UserFundingResponse,
        WsUserFundingResponse,
    };

    use super::{
        account_summary_from_state, build_exchange_info, open_order_from_response,
        position_from_state, track_pnl_record_from_funding, track_pnl_record_from_ws_funding,
    };

    #[test]
    fn converts_meta_asset_into_exchange_info() {
        let meta = MetaResponse {
            universe: vec![PerpAssetMeta {
                name: "BTC".to_string(),
                sz_decimals: 5,
                max_leverage: Some(40),
                only_isolated: None,
                margin_mode: None,
            }],
        };

        let info = build_exchange_info(&meta, "BTC").unwrap();

        let spec = info.instrument_spec;
        assert_eq!(
            spec.instrument(),
            &Instrument::new(Venue::Hyperliquid, "BTC")
        );
        assert_eq!(spec.price_tick(), 1.0);
        assert_eq!(
            spec.price_precision(),
            PricePrecision::significant_figures(1, 5)
        );
        assert_eq!(spec.position_qty_step().value(), 0.00001);
        assert_eq!(spec.min_position_qty().value(), 0.00001);
        assert_eq!(spec.min_notional().value(), 10.0);
    }

    #[test]
    fn exchange_info_uses_hyperliquid_effective_price_tick_for_eth() {
        let meta = MetaResponse {
            universe: vec![PerpAssetMeta {
                name: "ETH".to_string(),
                sz_decimals: 4,
                max_leverage: Some(25),
                only_isolated: None,
                margin_mode: None,
            }],
        };

        let info = build_exchange_info(&meta, "ETH").unwrap();

        assert_eq!(info.instrument_spec.price_tick(), 0.1);
    }

    #[test]
    fn exchange_info_reports_missing_asset() {
        let meta = MetaResponse { universe: vec![] };

        let error = build_exchange_info(&meta, "BTC").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("missing Hyperliquid asset `BTC`")
        );
    }

    #[test]
    fn converts_user_state_into_account_summary_and_position() {
        let state = ClearinghouseStateResponse {
            margin_summary: MarginSummary {
                account_value: "125.5".to_string(),
            },
            withdrawable: "100.25".to_string(),
            asset_positions: vec![AssetPosition {
                position: PositionData {
                    coin: "BTC".to_string(),
                    szi: "-0.02".to_string(),
                    entry_px: Some("65000.5".to_string()),
                    unrealized_pnl: "-3.25".to_string(),
                },
            }],
        };

        let summary = account_summary_from_state(&state).unwrap();
        let position = position_from_state(&state, "BTC").unwrap();
        let flat_position = position_from_state(&state, "ETH").unwrap();

        assert_eq!(summary.equity, 125.5);
        assert_eq!(summary.available, 100.25);
        assert_eq!(summary.unrealized_pnl, -3.25);
        assert_eq!(
            position,
            Position {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                qty: -0.02,
                avg_price: 65000.5,
                unrealized_pnl: -3.25,
                mark_price: None,
            }
        );
        assert_eq!(
            flat_position,
            Position {
                instrument: Instrument::new(Venue::Hyperliquid, "ETH"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
                mark_price: None,
            }
        );
    }

    #[test]
    fn converts_open_order_response_into_exchange_order() {
        let order = open_order_from_response(OpenOrderResponse {
            coin: "BTC".to_string(),
            oid: 12345,
            cloid: Some("0x11111111111111111111111111111111".to_string()),
            side: "B".to_string(),
            limit_px: "65000.5".to_string(),
            sz: "0.02".to_string(),
        })
        .unwrap();

        assert_eq!(
            order,
            ExchangeOrder {
                instrument: Instrument::new(Venue::Hyperliquid, "BTC"),
                order_id: "12345".to_string(),
                client_order_id: "0x11111111111111111111111111111111".to_string(),
                side: Side::Buy,
                price: 65000.5,
                qty: 0.02,
                filled_qty: 0.0,
                status: OrderStatus::New,
            }
        );
    }

    #[test]
    fn open_order_without_cloid_uses_order_id_as_client_order_id() {
        let order = open_order_from_response(OpenOrderResponse {
            coin: "BTC".to_string(),
            oid: 12345,
            cloid: None,
            side: "A".to_string(),
            limit_px: "65000.5".to_string(),
            sz: "0.02".to_string(),
        })
        .unwrap();

        assert_eq!(order.client_order_id, "12345");
        assert_eq!(order.side, Side::Sell);
    }

    #[test]
    fn rest_and_ws_funding_records_share_source_key() {
        let rest_record = track_pnl_record_from_funding(&UserFundingResponse {
            delta: FundingDeltaResponse {
                kind: "funding".to_string(),
                coin: "BTC".to_string(),
                usdc: "-0.15".to_string(),
            },
            time: 1_700_000_000_000,
            hash: "0xabc".to_string(),
        })
        .unwrap();
        let ws_record = track_pnl_record_from_ws_funding(&WsUserFundingResponse {
            coin: "BTC".to_string(),
            time: 1_700_000_000_000,
            usdc: "-0.15".to_string(),
        })
        .unwrap();

        assert_eq!(rest_record.source_key, ws_record.source_key);
        assert_eq!(
            rest_record.source_key.as_deref(),
            Some("hyperliquid:funding:BTC:1700000000000")
        );
        assert_eq!(rest_record.funding_fee, ws_record.funding_fee);
    }
}
