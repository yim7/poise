use chrono::{TimeZone, Utc};

use anyhow::{Context, Result, anyhow, ensure};

use crate::rest::models::{
    BinanceAccountSummaryInformation, BinanceAccountTrade, BinanceExchangeInfo,
    BinanceIncomeRecord, BinanceOpenOrder, BinanceOrderResponse, BinancePositionRisk,
};
use poise_application::exchange::{
    AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt, OrderStatus, Position,
};
use poise_application::exchange::{TrackPnlQuantity, TrackPnlRecord};
use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionQty};
use poise_core::track::{Instrument, Venue};

impl TryFrom<BinanceOrderResponse> for OrderReceipt {
    type Error = anyhow::Error;

    fn try_from(value: BinanceOrderResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            order_id: value.order_id.to_string(),
            client_order_id: value.client_order_id,
            filled_qty: PositionQty::new(
                value
                    .executed_qty
                    .as_deref()
                    .map(|qty| parse_decimal("executedQty", qty))
                    .transpose()?
                    .unwrap_or(0.0),
            )
            .map_err(anyhow::Error::msg)?,
            status: parse_order_status(&value.status)?,
        })
    }
}

impl TryFrom<BinancePositionRisk> for Position {
    type Error = anyhow::Error;

    fn try_from(value: BinancePositionRisk) -> Result<Self, Self::Error> {
        Ok(Self {
            instrument: Instrument::new(Venue::Binance, value.symbol),
            qty: parse_position_qty("positionAmt", &value.position_amt)?,
            avg_price: parse_decimal("entryPrice", &value.entry_price)?,
            unrealized_pnl: parse_decimal("unRealizedProfit", &value.unrealized_profit)?,
            mark_price: None,
        })
    }
}

impl BinanceAccountSummaryInformation {
    pub fn into_account_summary_snapshot(self) -> Result<AccountSummarySnapshot> {
        let valuation_asset = Asset::try_new(if self.multi_assets_margin {
            "USD"
        } else {
            "USDT"
        })
        .map_err(anyhow::Error::msg)?;
        let available_by_asset = self
            .assets
            .into_iter()
            .map(|balance| {
                let asset = Asset::try_new(balance.asset).map_err(anyhow::Error::msg)?;
                let value = parse_decimal(
                    &format!("assets[{}].availableBalance", asset.as_str()),
                    &balance.available_balance,
                )?;
                Ok((asset, value))
            })
            .collect::<Result<_>>()?;

        AccountSummarySnapshot::try_new(
            Amount::new(
                parse_decimal("totalMarginBalance", &self.total_margin_balance)?,
                valuation_asset.clone(),
            )
            .map_err(anyhow::Error::msg)?,
            Some(
                Amount::new(
                    parse_decimal("availableBalance", &self.available_balance)?,
                    valuation_asset.clone(),
                )
                .map_err(anyhow::Error::msg)?,
            ),
            available_by_asset,
            Some(
                Amount::new(
                    parse_decimal("totalUnrealizedProfit", &self.total_unrealized_profit)?,
                    valuation_asset,
                )
                .map_err(anyhow::Error::msg)?,
            ),
            Utc::now(),
        )
        .map_err(anyhow::Error::msg)
    }
}

impl TryFrom<BinanceOpenOrder> for ExchangeOrder {
    type Error = anyhow::Error;

    fn try_from(value: BinanceOpenOrder) -> Result<Self, Self::Error> {
        Ok(Self {
            instrument: Instrument::new(Venue::Binance, value.symbol),
            order_id: value.order_id.to_string(),
            client_order_id: value.client_order_id,
            side: parse_side(&value.side)?,
            price: parse_decimal("price", &value.price)?,
            qty: parse_position_qty("origQty", &value.orig_qty)?,
            filled_qty: PositionQty::new(
                value
                    .executed_qty
                    .as_deref()
                    .map(|qty| parse_decimal("executedQty", qty))
                    .transpose()?
                    .unwrap_or(0.0),
            )
            .map_err(anyhow::Error::msg)?,
            status: parse_order_status(&value.status)?,
        })
    }
}

pub(crate) fn track_pnl_record_from_trade(value: BinanceAccountTrade) -> Result<TrackPnlRecord> {
    let instrument = Instrument::new(Venue::Binance, &value.symbol);
    let settlement_asset = Asset::try_new(value.margin_asset).map_err(anyhow::Error::msg)?;
    let occurred_at = millis_to_utc("trade.time", value.time)?;
    let trade_id = value.id.to_string();
    let commission_asset = Asset::try_new(value.commission_asset).map_err(anyhow::Error::msg)?;

    Ok(TrackPnlRecord::trade(
        instrument,
        occurred_at,
        "binance:order_trade_update".to_string(),
        Some(format!(
            "binance:order_trade_update:{}:{trade_id}",
            value.symbol.to_lowercase()
        )),
        Some(value.order_id.to_string()),
        Some(trade_id),
        parse_side(&value.side)?,
        parse_decimal("price", &value.price)?,
        linear_pnl_quantity(
            &value.symbol,
            settlement_asset.as_str(),
            parse_position_qty("qty", &value.qty)?,
        )?,
        cash_amount(
            parse_decimal("realizedPnl", &value.realized_pnl)?,
            settlement_asset,
        )?,
        Some(cash_amount(
            parse_decimal("commission", &value.commission)?,
            commission_asset,
        )?),
    ))
}

pub(crate) fn track_pnl_record_from_funding(value: BinanceIncomeRecord) -> Result<TrackPnlRecord> {
    ensure!(
        value.income_type == "FUNDING_FEE",
        "unsupported Binance income type `{}`",
        value.income_type
    );
    let occurred_at = millis_to_utc("income.time", value.time)?;

    Ok(TrackPnlRecord::funding(
        Instrument::new(Venue::Binance, &value.symbol),
        occurred_at,
        "binance:funding_fee".to_string(),
        Some(format!(
            "binance:funding_fee:{}:{}",
            value.symbol.to_lowercase(),
            value.time
        )),
        cash_amount(
            parse_decimal("income", &value.income)?,
            Asset::try_new(value.asset).map_err(anyhow::Error::msg)?,
        )?,
    ))
}

fn cash_amount(value: f64, asset: Asset) -> Result<Amount> {
    Amount::new(value, asset).map_err(anyhow::Error::msg)
}

impl TryFrom<BinanceExchangeInfo> for ExchangeInfo {
    type Error = anyhow::Error;

    fn try_from(value: BinanceExchangeInfo) -> Result<Self, Self::Error> {
        let base_asset = Asset::try_new(value.base_asset).map_err(anyhow::Error::msg)?;
        let quote_asset = Asset::try_new(value.quote_asset).map_err(anyhow::Error::msg)?;
        let settlement_asset = Asset::try_new(value.margin_asset).map_err(anyhow::Error::msg)?;
        let instrument = Instrument::new(Venue::Binance, value.symbol);
        let price_filter = value
            .filters
            .iter()
            .find(|filter| filter.filter_type == "PRICE_FILTER")
            .context("missing PRICE_FILTER")?;
        let lot_size_filter = value
            .filters
            .iter()
            .find(|filter| filter.filter_type == "LOT_SIZE")
            .context("missing LOT_SIZE filter")?;
        let min_notional_filter = value
            .filters
            .iter()
            .find(|filter| filter.filter_type == "MIN_NOTIONAL")
            .context("missing MIN_NOTIONAL filter")?;

        let min_notional =
            parse_optional_decimal("notional", min_notional_filter.notional.as_deref())?;
        Ok(Self {
            instrument_spec: InstrumentSpec::try_linear(
                instrument,
                base_asset,
                quote_asset.clone(),
                settlement_asset,
                Default::default(),
                parse_optional_decimal("tickSize", price_filter.tick_size.as_deref())?,
                parse_optional_decimal("stepSize", lot_size_filter.step_size.as_deref())?,
                parse_optional_decimal("minQty", lot_size_filter.min_qty.as_deref())?,
                Amount::new(min_notional, quote_asset).map_err(anyhow::Error::msg)?,
            )
            .map_err(anyhow::Error::msg)?,
        })
    }
}

fn parse_side(value: &str) -> Result<poise_core::types::Side> {
    match value {
        "BUY" => Ok(poise_core::types::Side::Buy),
        "SELL" => Ok(poise_core::types::Side::Sell),
        other => Err(anyhow!("unsupported side: {other}")),
    }
}

pub(crate) fn parse_order_status(value: &str) -> Result<OrderStatus> {
    match value {
        "NEW" => Ok(OrderStatus::New),
        "PARTIALLY_FILLED" => Ok(OrderStatus::PartiallyFilled),
        "FILLED" => Ok(OrderStatus::Filled),
        "CANCELED" => Ok(OrderStatus::Canceled),
        "REJECTED" => Ok(OrderStatus::Rejected),
        "EXPIRED" => Ok(OrderStatus::Expired),
        other => Err(anyhow!("unsupported order status: {other}")),
    }
}

fn parse_optional_decimal(field: &str, value: Option<&str>) -> Result<f64> {
    let value = value.context(format!("missing {field}"))?;
    parse_decimal(field, value)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

fn parse_position_qty(field: &str, value: &str) -> Result<PositionQty> {
    PositionQty::new(parse_decimal(field, value)?).map_err(anyhow::Error::msg)
}

pub(crate) fn linear_pnl_quantity(
    symbol: &str,
    quote_asset: &str,
    qty: PositionQty,
) -> Result<TrackPnlQuantity> {
    let base_asset = symbol
        .strip_suffix(quote_asset)
        .filter(|asset| !asset.is_empty())
        .with_context(|| {
            format!("cannot determine Binance base asset from symbol `{symbol}` and quote `{quote_asset}`")
        })?;
    Ok(TrackPnlQuantity::base_asset(
        qty,
        Asset::try_new(base_asset).map_err(anyhow::Error::msg)?,
    ))
}

fn millis_to_utc(field: &str, value: i64) -> Result<chrono::DateTime<Utc>> {
    Utc.timestamp_millis_opt(value)
        .single()
        .with_context(|| format!("invalid Binance timestamp `{field}`: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::models::BinanceExchangeInfoResponse;
    use poise_core::instrument::{Amount, Asset, PositionBasis, PositionQty};
    use poise_core::types::{PriceRounding, Side};

    #[test]
    fn converts_order_response_into_order_receipt() {
        let payload = r#"
        {
            "orderId": 20072994037,
            "clientOrderId": "grid-order-001",
            "status": "NEW"
        }
        "#;

        let order: BinanceOrderResponse = serde_json::from_str(payload).unwrap();
        let receipt = OrderReceipt::try_from(order).unwrap();

        assert_eq!(
            receipt,
            OrderReceipt {
                order_id: "20072994037".to_string(),
                client_order_id: "grid-order-001".to_string(),
                filled_qty: PositionQty::default(),
                status: OrderStatus::New,
            }
        );
    }

    #[test]
    fn converts_position_risk_into_position() {
        let payload = r#"
        {
            "symbol": "BTCUSDT",
            "positionAmt": "0.250",
            "entryPrice": "65000.5",
            "unRealizedProfit": "123.45"
        }
        "#;

        let position: BinancePositionRisk = serde_json::from_str(payload).unwrap();
        let converted = Position::try_from(position).unwrap();

        assert_eq!(
            converted,
            Position {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                qty: PositionQty::new(0.25).unwrap(),
                avg_price: 65000.5,
                unrealized_pnl: 123.45,
                mark_price: None,
            }
        );
    }

    #[test]
    fn converts_account_information_into_account_summary_snapshot() {
        let payload = r#"
        {
            "multiAssetsMargin": false,
            "availableBalance": "9800.25",
            "totalMarginBalance": "12500.5",
            "totalUnrealizedProfit": "-120.75",
            "assets": [
                { "asset": "USDT", "availableBalance": "9800.25" }
            ]
        }
        "#;

        let account: BinanceAccountSummaryInformation = serde_json::from_str(payload).unwrap();
        let snapshot = account.into_account_summary_snapshot().unwrap();

        assert_eq!(snapshot.valuation_asset().as_str(), "USDT");
        assert_eq!(snapshot.equity().value(), 12_500.5);
        assert_eq!(snapshot.available().unwrap().value(), 9_800.25);
        assert_eq!(snapshot.unrealized_pnl().unwrap().value(), -120.75);
        assert_eq!(snapshot.available_for_asset("USDT"), Some(9_800.25));
    }

    #[test]
    fn multi_asset_account_totals_use_usd_valuation() {
        let payload = r#"
        {
            "multiAssetsMargin": true,
            "availableBalance": "19000.0",
            "totalMarginBalance": "20000.0",
            "totalUnrealizedProfit": "-100.0",
            "assets": [
                { "asset": "BTC", "availableBalance": "0.1" },
                { "asset": "USDT", "availableBalance": "5000.0" }
            ]
        }
        "#;

        let account: BinanceAccountSummaryInformation = serde_json::from_str(payload).unwrap();
        let snapshot = account.into_account_summary_snapshot().unwrap();

        assert_eq!(snapshot.valuation_asset().as_str(), "USD");
        assert_eq!(snapshot.available().unwrap().value(), 19_000.0);
        assert_eq!(snapshot.available_for_asset("BTC"), Some(0.1));
        assert_eq!(snapshot.available_for_asset("USDT"), Some(5_000.0));
    }

    #[test]
    fn converts_open_order_into_engine_order() {
        let payload = r#"
        {
            "symbol": "BTCUSDT",
            "orderId": 987654321,
            "clientOrderId": "grid-open-002",
            "side": "SELL",
            "price": "65123.4",
            "origQty": "0.010",
            "executedQty": "0.004",
            "status": "PARTIALLY_FILLED"
        }
        "#;

        let order: BinanceOpenOrder = serde_json::from_str(payload).unwrap();
        let converted = ExchangeOrder::try_from(order).unwrap();

        assert_eq!(
            converted,
            ExchangeOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: "987654321".to_string(),
                client_order_id: "grid-open-002".to_string(),
                side: Side::Sell,
                price: 65123.4,
                qty: PositionQty::new(0.01).unwrap(),
                filled_qty: PositionQty::new(0.004).unwrap(),
                status: OrderStatus::PartiallyFilled,
            }
        );
    }

    #[test]
    fn exchange_metadata_builds_complete_linear_instrument_spec() {
        let payload = r#"
        {
            "symbols": [
                {
                    "symbol": "PERPETUAL-42",
                    "baseAsset": "BTC",
                    "quoteAsset": "USD",
                    "marginAsset": "USDC",
                    "filters": [
                        {
                            "filterType": "PRICE_FILTER",
                            "tickSize": "0.10"
                        },
                        {
                            "filterType": "LOT_SIZE",
                            "minQty": "0.001",
                            "stepSize": "0.001"
                        },
                        {
                            "filterType": "MIN_NOTIONAL",
                            "notional": "100"
                        }
                    ]
                }
            ]
        }
        "#;

        let response: BinanceExchangeInfoResponse = serde_json::from_str(payload).unwrap();
        let converted =
            ExchangeInfo::try_from(response.symbols.into_iter().next().unwrap()).unwrap();

        let spec = converted.instrument_spec;
        assert_eq!(
            spec.instrument(),
            &Instrument::new(Venue::Binance, "PERPETUAL-42")
        );
        assert_eq!(spec.base_asset(), &Asset::try_new("BTC").unwrap());
        assert_eq!(spec.quote_asset(), &Asset::try_new("USD").unwrap());
        assert_eq!(spec.notional_asset(), &Asset::try_new("USD").unwrap());
        assert_eq!(spec.settlement_asset(), &Asset::try_new("USDC").unwrap());
        assert_eq!(spec.position_basis(), &PositionBasis::BaseAsset);
        assert_eq!(spec.price_tick(), 0.1);
        assert_eq!(spec.position_qty_step().value(), 0.001);
        assert_eq!(spec.min_position_qty().value(), 0.001);
        assert_eq!(spec.min_notional().value(), 100.0);

        let canonical = PositionQty::new(0.02).unwrap();
        let notional = spec
            .notional_from_position_qty(canonical, 65_000.0)
            .unwrap();
        assert_eq!(
            notional,
            Amount::new(1_300.0, Asset::try_new("USD").unwrap()).unwrap()
        );
        assert_eq!(
            spec.position_qty_for_notional(notional, 65_000.0).unwrap(),
            canonical
        );
        assert_eq!(
            spec.base_exposure_from_position_qty(canonical, 65_000.0)
                .unwrap(),
            Amount::new(0.02, Asset::try_new("BTC").unwrap()).unwrap()
        );
        assert!(
            (spec
                .normalize_position_qty(PositionQty::new(0.0209).unwrap())
                .value()
                - 0.02)
                .abs()
                < 1e-12
        );
        assert!((spec.normalize_price(65_000.19, PriceRounding::Down) - 65_000.1).abs() < 1e-9);
    }
}
