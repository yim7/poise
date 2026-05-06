use poise_core::types::Side;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::protocol::{
    BybitOrderStatus, deserialize_f64, deserialize_i64, deserialize_optional_f64,
    deserialize_optional_side, deserialize_order_status, deserialize_side,
};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BybitEnvelope {
    #[serde(rename = "retCode")]
    pub ret_code: i64,
    #[serde(rename = "retMsg")]
    pub ret_msg: Option<String>,
    #[serde(default)]
    result: serde_json::Value,
}

impl BybitEnvelope {
    pub(crate) fn deserialize_result<T>(self) -> serde_json::Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.result)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InstrumentInfoResult {
    pub list: Vec<LinearInstrumentInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LinearInstrumentInfo {
    pub symbol: String,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PriceFilter {
    #[serde(
        default,
        rename = "tickSize",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub tick_size: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LotSizeFilter {
    #[serde(
        default,
        rename = "qtyStep",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub qty_step: Option<f64>,
    #[serde(
        default,
        rename = "minOrderQty",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub min_order_qty: Option<f64>,
    #[serde(
        default,
        rename = "minNotionalValue",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub min_notional_value: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WalletBalanceResult {
    pub list: Vec<UnifiedWalletBalance>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UnifiedWalletBalance {
    #[serde(rename = "accountType")]
    pub account_type: Option<String>,
    #[serde(
        default,
        rename = "totalEquity",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub total_equity: Option<f64>,
    #[serde(
        default,
        rename = "totalAvailableBalance",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub total_available_balance: Option<f64>,
    #[serde(
        default,
        rename = "totalPerpUPL",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub total_perp_upl: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServerTimeResult {
    #[serde(rename = "timeSecond", deserialize_with = "deserialize_i64")]
    pub time_second: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOrderRequestBody {
    pub category: &'static str,
    pub symbol: String,
    pub side: String,
    #[serde(rename = "orderType")]
    pub order_type: &'static str,
    pub qty: String,
    pub price: String,
    #[serde(rename = "timeInForce")]
    pub time_in_force: &'static str,
    #[serde(rename = "positionIdx")]
    pub position_idx: i64,
    #[serde(rename = "orderLinkId")]
    pub order_link_id: String,
    #[serde(rename = "reduceOnly")]
    pub reduce_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOrderResult {
    #[serde(rename = "orderId")]
    pub order_id: String,
    #[serde(default, rename = "orderLinkId")]
    pub order_link_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelOrderRequestBody {
    pub category: &'static str,
    pub symbol: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "orderId")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "orderLinkId")]
    pub order_link_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelAllRequestBody {
    pub category: &'static str,
    pub symbol: String,
    #[serde(rename = "orderFilter")]
    pub order_filter: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetLeverageRequestBody {
    pub category: &'static str,
    pub symbol: String,
    #[serde(rename = "buyLeverage")]
    pub buy_leverage: String,
    #[serde(rename = "sellLeverage")]
    pub sell_leverage: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelAllResult {
    #[serde(rename = "list")]
    pub _list: Vec<CancelAllOrderAck>,
    pub success: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelAllOrderAck {
    #[serde(rename = "orderId")]
    pub _order_id: String,
    #[serde(default, rename = "orderLinkId")]
    pub _order_link_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionListResult {
    pub list: Vec<PositionSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PositionSnapshot {
    pub symbol: String,
    #[serde(default, deserialize_with = "deserialize_optional_side")]
    pub side: Option<Side>,
    #[serde(deserialize_with = "deserialize_f64")]
    pub size: f64,
    #[serde(default, deserialize_with = "deserialize_optional_f64")]
    pub leverage: Option<f64>,
    #[serde(
        default,
        rename = "avgPrice",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub avg_price: Option<f64>,
    #[serde(
        default,
        rename = "unrealisedPnl",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub unrealised_pnl: Option<f64>,
    #[serde(rename = "positionIdx", deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenOrderListResult {
    pub list: Vec<OpenOrderSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenOrderSnapshot {
    pub symbol: String,
    pub order_id: String,
    #[serde(default, rename = "orderLinkId")]
    pub order_link_id: Option<String>,
    #[serde(deserialize_with = "deserialize_side")]
    pub side: Side,
    #[serde(deserialize_with = "deserialize_f64")]
    pub price: f64,
    #[serde(deserialize_with = "deserialize_f64")]
    pub qty: f64,
    #[serde(
        default,
        rename = "cumExecQty",
        deserialize_with = "deserialize_optional_f64"
    )]
    pub cum_exec_qty: Option<f64>,
    #[serde(rename = "orderStatus", deserialize_with = "deserialize_order_status")]
    pub order_status: BybitOrderStatus,
    #[serde(default, rename = "stopOrderType")]
    pub stop_order_type: Option<String>,
    #[serde(rename = "positionIdx", deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

#[cfg(test)]
mod tests {
    use poise_core::types::Side;

    use super::*;
    use crate::protocol::BybitOrderStatus;

    #[test]
    fn deserializes_server_time_second_from_bybit_string_response() {
        let response: BybitEnvelope = serde_json::from_str(
            r#"{"retCode":0,"retMsg":"OK","result":{"timeSecond":"1775928345","timeNano":"1775928345295229586"},"retExtInfo":{},"time":1775928345295}"#,
        )
        .unwrap();

        assert_eq!(response.ret_code, 0);
        assert_eq!(response.ret_msg.as_deref(), Some("OK"));
        assert_eq!(
            response
                .deserialize_result::<ServerTimeResult>()
                .unwrap()
                .time_second,
            1_775_928_345
        );
    }

    #[test]
    fn envelope_defers_result_deserialization_until_success_is_known() {
        let response: BybitEnvelope = serde_json::from_str(
            r#"{"retCode":110007,"retMsg":"available balance is insufficient","result":{}}"#,
        )
        .unwrap();

        assert_eq!(response.ret_code, 110007);
        assert_eq!(
            response.ret_msg.as_deref(),
            Some("available balance is insufficient")
        );
    }

    #[test]
    fn deserializes_position_snapshot_into_typed_fields() {
        let response: BybitEnvelope = serde_json::from_str(
            r#"{
                "retCode": 0,
                "retMsg": "OK",
                "result": {
                    "list": [{
                        "symbol": "BTCUSDT",
                        "side": "",
                        "size": "0",
                        "leverage": "10",
                        "avgPrice": "",
                        "unrealisedPnl": "",
                        "positionIdx": 0
                    }]
                }
            }"#,
        )
        .unwrap();
        let result = response.deserialize_result::<PositionListResult>().unwrap();

        let snapshot = &result.list[0];
        assert_eq!(snapshot.symbol, "BTCUSDT");
        assert_eq!(snapshot.side, None);
        assert_eq!(snapshot.size, 0.0);
        assert_eq!(snapshot.leverage, Some(10.0));
        assert_eq!(snapshot.avg_price, None);
        assert_eq!(snapshot.unrealised_pnl, None);
        assert_eq!(snapshot.position_idx, 0);
    }

    #[test]
    fn deserializes_open_order_snapshot_into_typed_fields() {
        let response: BybitEnvelope = serde_json::from_str(
            r#"{
                "retCode": 0,
                "retMsg": "OK",
                "result": {
                    "list": [{
                        "symbol": "BTCUSDT",
                        "orderId": "12345",
                        "orderLinkId": "client-1",
                        "side": "Buy",
                        "price": "65000.5",
                        "qty": "0.25",
                        "orderStatus": "PartiallyFilled",
                        "positionIdx": 0
                    }]
                }
            }"#,
        )
        .unwrap();
        let result = response
            .deserialize_result::<OpenOrderListResult>()
            .unwrap();

        let snapshot = &result.list[0];
        assert_eq!(snapshot.symbol, "BTCUSDT");
        assert_eq!(snapshot.order_id, "12345");
        assert_eq!(snapshot.side, Side::Buy);
        assert_eq!(snapshot.price, 65000.5);
        assert_eq!(snapshot.qty, 0.25);
        assert_eq!(snapshot.order_status, BybitOrderStatus::PartiallyFilled);
        assert_eq!(snapshot.position_idx, 0);
    }
}
