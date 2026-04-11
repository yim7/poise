use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BybitResponse<T> {
    #[serde(rename = "retCode")]
    pub ret_code: i64,
    #[serde(rename = "retMsg")]
    pub ret_msg: Option<String>,
    pub result: T,
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
    #[serde(rename = "tickSize")]
    pub tick_size: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LotSizeFilter {
    #[serde(rename = "qtyStep")]
    pub qty_step: Option<String>,
    #[serde(rename = "minOrderQty")]
    pub min_order_qty: Option<String>,
    #[serde(rename = "minNotionalValue")]
    pub min_notional_value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WalletBalanceResult {
    pub list: Vec<UnifiedWalletBalance>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UnifiedWalletBalance {
    #[serde(rename = "accountType")]
    pub account_type: Option<String>,
    #[serde(rename = "totalEquity")]
    pub total_equity: Option<String>,
    #[serde(rename = "totalAvailableBalance")]
    pub total_available_balance: Option<String>,
    #[serde(rename = "totalPerpUPL")]
    pub total_perp_upl: Option<String>,
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
    #[serde(default)]
    pub side: Option<String>,
    #[serde(deserialize_with = "deserialize_string")]
    pub size: String,
    #[serde(rename = "avgPrice", deserialize_with = "deserialize_string")]
    pub avg_price: String,
    #[serde(rename = "unrealisedPnl", deserialize_with = "deserialize_string")]
    pub unrealised_pnl: String,
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
    #[serde(rename = "orderId", deserialize_with = "deserialize_string")]
    pub order_id: String,
    #[serde(default, rename = "orderLinkId")]
    pub order_link_id: Option<String>,
    #[serde(deserialize_with = "deserialize_string")]
    pub side: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub price: String,
    #[serde(deserialize_with = "deserialize_string")]
    pub qty: String,
    #[serde(rename = "orderStatus", deserialize_with = "deserialize_string")]
    pub order_status: String,
    #[serde(default, rename = "stopOrderType")]
    pub stop_order_type: Option<String>,
    #[serde(rename = "positionIdx", deserialize_with = "deserialize_i64")]
    pub position_idx: i64,
}

pub(crate) fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        other => Err(Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

pub(crate) fn deserialize_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(value) => value
            .as_i64()
            .ok_or_else(|| Error::custom("expected integer number")),
        serde_json::Value::String(value) => value
            .parse::<i64>()
            .map_err(|error| Error::custom(format!("invalid integer `{value}`: {error}"))),
        other => Err(Error::custom(format!("expected integer, got {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_server_time_second_from_bybit_string_response() {
        let response: BybitResponse<ServerTimeResult> = serde_json::from_str(
            r#"{"retCode":0,"retMsg":"OK","result":{"timeSecond":"1775928345","timeNano":"1775928345295229586"},"retExtInfo":{},"time":1775928345295}"#,
        )
        .unwrap();

        assert_eq!(response.result.time_second, 1_775_928_345);
    }
}
