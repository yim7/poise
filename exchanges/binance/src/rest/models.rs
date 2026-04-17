use serde::Deserialize;

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
pub struct BinanceAccountSummaryInformation {
    #[serde(rename = "availableBalance")]
    pub available_balance: String,
    #[serde(rename = "totalMarginBalance")]
    pub total_margin_balance: String,
    #[serde(rename = "totalUnrealizedProfit")]
    pub total_unrealized_profit: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceSymbolConfiguration {
    pub symbol: String,
    pub leverage: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceLeverageChangeResponse {
    pub leverage: u32,
    pub symbol: String,
    #[serde(rename = "maxNotionalValue")]
    pub max_notional_value: String,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    pub server_time: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceErrorResponse {
    pub code: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}
