use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use grid_engine::ports::{ExchangeInfo, ExchangeOrder, OrderReceipt, OrderStatus, Position};

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceOrderResponse {
    #[serde(rename = "orderId")]
    pub order_id: u64,
    #[serde(rename = "clientOrderId")]
    pub client_order_id: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinancePositionRisk {
    pub symbol: String,
    #[serde(rename = "positionAmt")]
    pub position_amt: String,
    #[serde(rename = "entryPrice")]
    pub entry_price: String,
    #[serde(rename = "unRealizedProfit")]
    pub unrealized_profit: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceOpenOrder {
    pub symbol: String,
    #[serde(rename = "orderId")]
    pub order_id: u64,
    #[serde(rename = "clientOrderId")]
    pub client_order_id: String,
    pub side: String,
    pub price: String,
    #[serde(rename = "origQty")]
    pub orig_qty: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceExchangeInfoResponse {
    pub symbols: Vec<BinanceExchangeInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceExchangeInfo {
    pub symbol: String,
    pub filters: Vec<BinanceSymbolFilter>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceSymbolFilter {
    #[serde(rename = "filterType")]
    pub filter_type: String,
    #[serde(rename = "tickSize")]
    pub tick_size: Option<String>,
    #[serde(rename = "minQty")]
    pub min_qty: Option<String>,
    #[serde(rename = "stepSize")]
    pub step_size: Option<String>,
    pub notional: Option<String>,
}

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
            symbol: value.symbol,
            qty: parse_decimal("positionAmt", &value.position_amt)?,
            avg_price: parse_decimal("entryPrice", &value.entry_price)?,
            unrealized_pnl: parse_decimal("unRealizedProfit", &value.unrealized_profit)?,
        })
    }
}

impl TryFrom<BinanceOpenOrder> for ExchangeOrder {
    type Error = anyhow::Error;

    fn try_from(value: BinanceOpenOrder) -> Result<Self, Self::Error> {
        Ok(Self {
            symbol: value.symbol,
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
            symbol: value.symbol,
            rules: grid_core::types::ExchangeRules {
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
            },
        })
    }
}

fn parse_side(value: &str) -> Result<grid_core::types::Side> {
    match value {
        "BUY" => Ok(grid_core::types::Side::Buy),
        "SELL" => Ok(grid_core::types::Side::Sell),
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
    use grid_core::types::{ExchangeRules, Side};

    use super::*;

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
                symbol: "BTCUSDT".to_string(),
                qty: 0.25,
                avg_price: 65000.5,
                unrealized_pnl: 123.45,
            }
        );
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
                symbol: "BTCUSDT".to_string(),
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
                symbol: "BTCUSDT".to_string(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.001,
                    min_qty: 0.001,
                    min_notional: 100.0,
                },
            }
        );
    }
}
