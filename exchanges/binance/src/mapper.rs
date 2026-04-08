use chrono::Utc;

use anyhow::{Context, Result, anyhow};

use crate::rest::models::{
    BinanceAccountSummaryInformation, BinanceExchangeInfo, BinanceOpenOrder, BinanceOrderResponse,
    BinancePositionRisk, BinanceSymbolConfiguration,
};
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo, ExchangeOrder, OrderReceipt,
    OrderStatus, Position,
};
use poise_engine::track::{Instrument, Venue};

impl TryFrom<BinanceOrderResponse> for OrderReceipt {
    type Error = anyhow::Error;

    fn try_from(value: BinanceOrderResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            order_id: value.order_id.to_string(),
            client_order_id: value.client_order_id,
            status: parse_order_status(&value.status)?,
        })
    }
}

impl TryFrom<BinancePositionRisk> for Position {
    type Error = anyhow::Error;

    fn try_from(value: BinancePositionRisk) -> Result<Self, Self::Error> {
        Ok(Self {
            instrument: Instrument::new(Venue::Binance, value.symbol),
            qty: parse_decimal("positionAmt", &value.position_amt)?,
            avg_price: parse_decimal("entryPrice", &value.entry_price)?,
            unrealized_pnl: parse_decimal("unRealizedProfit", &value.unrealized_profit)?,
        })
    }
}

impl BinanceAccountSummaryInformation {
    pub fn into_account_summary_snapshot(self) -> Result<AccountSummarySnapshot> {
        Ok(AccountSummarySnapshot {
            equity: parse_decimal("totalMarginBalance", &self.total_margin_balance)?,
            available: parse_decimal("availableBalance", &self.available_balance)?,
            unrealized_pnl: parse_decimal("totalUnrealizedProfit", &self.total_unrealized_profit)?,
            observed_at: Utc::now(),
        })
    }
}

pub(crate) fn build_account_capacity_snapshot(
    account: BinanceAccountSummaryInformation,
    symbol_config: BinanceSymbolConfiguration,
) -> Result<AccountCapacitySnapshot> {
    let available_balance = parse_decimal("availableBalance", &account.available_balance)?;

    Ok(AccountCapacitySnapshot {
        max_increase_notional: available_balance * symbol_config.leverage as f64,
    })
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
            qty: parse_decimal("origQty", &value.orig_qty)?,
            realized_pnl: 0.0,
            status: parse_order_status(&value.status)?,
        })
    }
}

impl TryFrom<BinanceExchangeInfo> for ExchangeInfo {
    type Error = anyhow::Error;

    fn try_from(value: BinanceExchangeInfo) -> Result<Self, Self::Error> {
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

        Ok(Self {
            instrument,
            rules: poise_core::types::ExchangeRules {
                price_tick: parse_optional_decimal("tickSize", price_filter.tick_size.as_deref())?,
                quantity_step: parse_optional_decimal(
                    "stepSize",
                    lot_size_filter.step_size.as_deref(),
                )?,
                min_qty: parse_optional_decimal("minQty", lot_size_filter.min_qty.as_deref())?,
                min_notional: parse_optional_decimal(
                    "notional",
                    min_notional_filter.notional.as_deref(),
                )?,
                // exchangeInfo does not include fee rates; default to VIP0 until
                // commissionRate is wired in.
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0004,
            },
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

#[cfg(test)]
mod tests {
    use poise_core::types::{ExchangeRules, Side};
    use poise_engine::ports::AccountSummarySnapshot;

    use super::*;
    use crate::rest::models::BinanceExchangeInfoResponse;

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
                qty: 0.25,
                avg_price: 65000.5,
                unrealized_pnl: 123.45,
            }
        );
    }

    #[test]
    fn converts_account_information_into_account_summary_snapshot() {
        let payload = r#"
        {
            "availableBalance": "9800.25",
            "totalMarginBalance": "12500.5",
            "totalUnrealizedProfit": "-120.75"
        }
        "#;

        let account: BinanceAccountSummaryInformation = serde_json::from_str(payload).unwrap();
        let snapshot = account.into_account_summary_snapshot().unwrap();

        assert_eq!(
            snapshot,
            AccountSummarySnapshot {
                equity: 12_500.5,
                available: 9_800.25,
                unrealized_pnl: -120.75,
                observed_at: snapshot.observed_at,
            }
        );
    }

    #[test]
    fn builds_account_capacity_snapshot_from_account_summary_and_symbol_config() {
        let account_payload = r#"
        {
            "availableBalance": "100.5",
            "totalMarginBalance": "125.25",
            "totalUnrealizedProfit": "4.5"
        }
        "#;
        let symbol_config_payload = r#"
        {
            "symbol": "BTCUSDT",
            "leverage": 20
        }
        "#;

        let account: BinanceAccountSummaryInformation =
            serde_json::from_str(account_payload).unwrap();
        let symbol_config: BinanceSymbolConfiguration =
            serde_json::from_str(symbol_config_payload).unwrap();
        let snapshot = build_account_capacity_snapshot(account, symbol_config).unwrap();

        assert_eq!(snapshot.max_increase_notional, 2010.0);
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
                qty: 0.01,
                realized_pnl: 0.0,
                status: OrderStatus::PartiallyFilled,
            }
        );
    }

    #[test]
    fn converts_exchange_info_into_engine_rules() {
        let payload = r#"
        {
            "symbols": [
                {
                    "symbol": "BTCUSDT",
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

        assert_eq!(
            converted,
            ExchangeInfo {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 100.0,
                    maker_fee_rate: 0.0002,
                    taker_fee_rate: 0.0004,
                },
            }
        );
    }
}
