use poise_core::types::Side;
use serde::Deserialize;

use crate::protocol::{
    BybitOrderStatus, deserialize_f64, deserialize_i64, deserialize_optional_f64,
    deserialize_optional_side, deserialize_order_status, deserialize_side,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTickerMessage {
    pub topic: String,
    pub ts: i64,
    pub data: PublicTickerData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTickerData {
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_f64")]
    pub mark_price: Option<f64>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_f64")]
    pub bid1_price: Option<f64>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_f64")]
    pub ask1_price: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrderTopicMessage {
    pub topic: String,
    pub creation_time: i64,
    pub data: Vec<OrderUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionTopicMessage {
    pub topic: String,
    pub creation_time: i64,
    pub data: Vec<ExecutionUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionUpdate {
    pub symbol: String,
    pub exec_id: String,
    #[serde(deserialize_with = "deserialize_f64")]
    pub exec_pnl: f64,
    #[serde(deserialize_with = "deserialize_f64")]
    pub exec_fee: f64,
    #[serde(default)]
    pub fee_currency: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrderUpdate {
    pub symbol: String,
    pub order_id: String,
    #[serde(default)]
    pub order_link_id: Option<String>,
    #[serde(deserialize_with = "deserialize_side")]
    pub side: Side,
    #[serde(deserialize_with = "deserialize_f64")]
    pub price: f64,
    #[serde(deserialize_with = "deserialize_f64")]
    pub qty: f64,
    #[serde(deserialize_with = "deserialize_order_status")]
    pub order_status: BybitOrderStatus,
    #[serde(default)]
    pub stop_order_type: Option<String>,
    #[serde(deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionTopicMessage {
    pub topic: String,
    pub creation_time: i64,
    pub data: Vec<PositionUpdate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionUpdate {
    pub symbol: String,
    #[serde(default, deserialize_with = "deserialize_optional_side")]
    pub side: Option<Side>,
    #[serde(deserialize_with = "deserialize_f64")]
    pub size: f64,
    #[serde(
        default,
        rename = "entryPrice",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub entry_price: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    pub unrealised_pnl: Option<f64>,
    #[serde(deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

#[cfg(test)]
mod tests {
    use poise_core::types::Side;

    use super::*;
    use crate::protocol::BybitOrderStatus;

    #[test]
    fn deserializes_execution_update_into_numeric_fields() {
        let message: ExecutionTopicMessage = serde_json::from_str(
            r#"{
                "topic": "execution.linear",
                "creationTime": 1700000000000,
                "data": [{
                    "symbol": "BTCUSDT",
                    "execId": "exec-1",
                    "execPnl": "12.34",
                    "execFee": "3.21",
                    "feeCurrency": "USDT"
                }]
            }"#,
        )
        .unwrap();

        let update = &message.data[0];
        assert_eq!(update.symbol, "BTCUSDT");
        assert_eq!(update.exec_id, "exec-1");
        assert_eq!(update.exec_pnl, 12.34);
        assert_eq!(update.exec_fee, 3.21);
        assert_eq!(update.fee_currency.as_deref(), Some("USDT"));
    }

    #[test]
    fn deserializes_order_update_into_typed_fields() {
        let message: OrderTopicMessage = serde_json::from_str(
            r#"{
                "topic": "order.linear",
                "creationTime": 1700000000000,
                "data": [{
                    "symbol": "BTCUSDT",
                    "orderId": "123",
                    "orderLinkId": "client-1",
                    "side": "Buy",
                    "price": "64000.10",
                    "qty": "0.010",
                    "orderStatus": "New",
                    "positionIdx": 0
                }]
            }"#,
        )
        .unwrap();

        let update = &message.data[0];
        assert_eq!(update.symbol, "BTCUSDT");
        assert_eq!(update.order_id, "123");
        assert_eq!(update.side, Side::Buy);
        assert_eq!(update.price, 64000.10);
        assert_eq!(update.qty, 0.010);
        assert_eq!(update.order_status, BybitOrderStatus::New);
        assert_eq!(update.position_idx, 0);
    }

    #[test]
    fn deserializes_position_update_blank_numeric_fields_to_none() {
        let message: PositionTopicMessage = serde_json::from_str(
            r#"{
                "topic": "position.linear",
                "creationTime": 1700000000000,
                "data": [{
                    "symbol": "BTCUSDT",
                    "side": "",
                    "size": "0",
                    "entryPrice": "",
                    "unrealisedPnl": "",
                    "positionIdx": 0
                }]
            }"#,
        )
        .unwrap();

        let update = &message.data[0];
        assert_eq!(update.symbol, "BTCUSDT");
        assert_eq!(update.side, None);
        assert_eq!(update.size, 0.0);
        assert_eq!(update.entry_price, None);
        assert_eq!(update.unrealised_pnl, None);
        assert_eq!(update.position_idx, 0);
    }
}
