use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceOrderResponse {
    #[serde(rename = "orderId")]
    pub order_id: u64,
    #[serde(rename = "clientOrderId")]
    pub client_order_id: String,
    #[serde(default, rename = "executedQty")]
    pub executed_qty: Option<String>,
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
pub struct BinanceMarkPrice {
    pub symbol: String,
    #[serde(rename = "markPrice")]
    pub mark_price: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceAccountSummaryInformation {
    #[serde(default, rename = "multiAssetsMargin")]
    pub multi_assets_margin: bool,
    #[serde(rename = "availableBalance")]
    pub available_balance: String,
    #[serde(rename = "totalMarginBalance")]
    pub total_margin_balance: String,
    #[serde(rename = "totalUnrealizedProfit")]
    pub total_unrealized_profit: String,
    #[serde(default)]
    pub assets: Vec<BinanceAssetBalance>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceAssetBalance {
    pub asset: String,
    #[serde(rename = "availableBalance")]
    pub available_balance: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinancePositionMode {
    #[serde(rename = "dualSidePosition")]
    pub dual_side_position: bool,
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
    #[serde(default, rename = "executedQty")]
    pub executed_qty: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceAccountTrade {
    pub commission: String,
    #[serde(rename = "commissionAsset")]
    pub commission_asset: String,
    pub id: u64,
    #[serde(rename = "orderId")]
    pub order_id: u64,
    pub price: String,
    pub qty: String,
    #[serde(rename = "marginAsset")]
    pub margin_asset: String,
    #[serde(rename = "realizedPnl")]
    pub realized_pnl: String,
    pub side: String,
    pub symbol: String,
    pub time: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceIncomeRecord {
    pub symbol: String,
    #[serde(rename = "incomeType")]
    pub income_type: String,
    pub income: String,
    pub asset: String,
    pub time: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceExchangeInfoResponse {
    pub symbols: Vec<BinanceExchangeInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BinanceExchangeInfo {
    pub symbol: String,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    #[serde(rename = "marginAsset")]
    pub margin_asset: String,
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
    #[serde(default)]
    pub msg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}
