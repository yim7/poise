use serde::Deserialize;

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
    #[serde(rename = "timeSecond")]
    pub time_second: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServerTimeResponse {
    pub result: ServerTimeResult,
}

#[cfg(test)]
mod tests {}
