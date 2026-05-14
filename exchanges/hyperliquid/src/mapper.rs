use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use poise_core::track::{Instrument, Venue};
use poise_core::types::{ExchangeRules, Side};
use poise_engine::ports::{
    AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderStatus, Position,
};

use crate::rest::models::{ClearinghouseStateResponse, MetaResponse, OpenOrderResponse};
use crate::rules::{perp_price_precision, representative_perp_price_tick};
const MIN_NOTIONAL_USD: f64 = 10.0;
const VIP0_MAKER_FEE_RATE: f64 = 0.00015;
const VIP0_TAKER_FEE_RATE: f64 = 0.00045;

pub(crate) fn build_exchange_info(meta: &MetaResponse, symbol: &str) -> Result<ExchangeInfo> {
    let asset = meta
        .universe
        .iter()
        .find(|asset| asset.name == symbol)
        .ok_or_else(|| anyhow!("missing Hyperliquid asset `{symbol}`"))?;
    let quantity_step = decimal_step(asset.sz_decimals as i32);

    Ok(ExchangeInfo {
        instrument: Instrument::new(Venue::Hyperliquid, symbol),
        rules: ExchangeRules {
            price_tick: representative_perp_price_tick(asset.sz_decimals),
            price_precision: perp_price_precision(asset.sz_decimals),
            quantity_step,
            min_qty: quantity_step,
            min_notional: MIN_NOTIONAL_USD,
            maker_fee_rate: VIP0_MAKER_FEE_RATE,
            taker_fee_rate: VIP0_TAKER_FEE_RATE,
        },
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
        .parse::<f64>()
        .with_context(|| format!("invalid Hyperliquid decimal `{field}`: {value}"))
}

#[cfg(test)]
mod tests {
    use poise_core::track::{Instrument, Venue};
    use poise_core::types::{ExchangeRules, PricePrecision, Side};
    use poise_engine::ports::{ExchangeOrder, OrderStatus, Position};

    use crate::rest::models::{
        AssetPosition, ClearinghouseStateResponse, MarginSummary, MetaResponse, OpenOrderResponse,
        PerpAssetMeta, PositionData,
    };

    use super::{
        account_summary_from_state, build_exchange_info, open_order_from_response,
        position_from_state,
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

        assert_eq!(info.instrument, Instrument::new(Venue::Hyperliquid, "BTC"));
        assert_eq!(
            info.rules,
            ExchangeRules {
                price_tick: 1.0,
                price_precision: PricePrecision::significant_figures(1, 5),
                quantity_step: 0.00001,
                min_qty: 0.00001,
                min_notional: 10.0,
                maker_fee_rate: 0.00015,
                taker_fee_rate: 0.00045,
            }
        );
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

        assert_eq!(info.rules.price_tick, 0.1);
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
            }
        );
        assert_eq!(
            flat_position,
            Position {
                instrument: Instrument::new(Venue::Hyperliquid, "ETH"),
                qty: 0.0,
                avg_price: 0.0,
                unrealized_pnl: 0.0,
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
}
