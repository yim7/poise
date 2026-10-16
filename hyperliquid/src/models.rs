//! `/info` 与 `/exchange` 的响应。
//!
//! `/info` 的响应形状稳定，用结构体接；`/exchange` 的状态有 `resting`/`filled`/`error`
//! 三种形态、还会随交易所版本增减，用 `serde_json::Value` 按需取字段更稳。
//!
//! 所有数值都是字符串，解析失败一律当成「看不懂」，不去猜。

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct MetaResponse {
    pub universe: Vec<AssetMetaResponse>,
    /// 保证金分档表：`[[表编号, 表], …]`。
    #[serde(rename = "marginTables", default)]
    pub margin_tables: Vec<(u32, MarginTableResponse)>,
    /// 这个市场的抵押币（token 编号）；主市场的 `meta` 没有这一项，是 USDC。
    #[serde(rename = "collateralToken", default)]
    pub collateral_token: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct MarginTableResponse {
    #[serde(rename = "marginTiers")]
    pub margin_tiers: Vec<MarginTierResponse>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct MarginTierResponse {
    /// 这一档的名义价值下限（美元）。
    #[serde(rename = "lowerBound")]
    pub lower_bound: String,
    #[serde(rename = "maxLeverage")]
    pub max_leverage: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AssetMetaResponse {
    pub name: String,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u32,
    #[serde(rename = "maxLeverage", default)]
    pub max_leverage: Option<u32>,
    /// 这个标的用哪张保证金分档表；分档表里找不到时只有一档，按 `maxLeverage` 算。
    #[serde(rename = "marginTableId", default)]
    pub margin_table_id: Option<u32>,
    /// 第三方市场（HIP-3）部署者的费用系数，例如 `"1.0"`；主市场没有。
    #[serde(rename = "deployerFeeScale", default)]
    pub deployer_fee_scale: Option<String>,
    /// 第三方市场的增长模式：`"enabled"` 时手续费打一折；主市场没有。
    #[serde(rename = "growthMode", default)]
    pub growth_mode: Option<String>,
}

/// `perpDexs` 的一项：第三方市场（HIP-3）。主市场那一项是 `null`。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PerpDexResponse {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AssetContext {
    #[serde(rename = "markPx")]
    pub mark_px: String,
    /// 当前资金费率（每小时），可能多于 8 位小数。
    #[serde(default)]
    pub funding: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ClearinghouseState {
    /// 交易所没给仓位列表就是读不出仓位：不能当成空仓。
    #[serde(rename = "assetPositions")]
    pub asset_positions: Vec<AssetPosition>,
    /// 交易所生成这份状态的时刻（毫秒）。
    #[serde(default)]
    pub time: Option<i64>,
    /// 全仓仓位的维持保证金占用合计。缺了就读不出账户比率。
    #[serde(rename = "crossMaintenanceMarginUsed", default)]
    pub cross_maintenance_margin_used: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AssetPosition {
    pub position: PositionData,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PositionData {
    pub coin: String,
    /// 带符号的持仓数量，多头为正。
    pub szi: String,
    #[serde(default)]
    pub leverage: Option<LeverageData>,
    /// 这个仓位占用的保证金。逐仓仓位按它从余额里扣（账户比率的分母）。
    #[serde(rename = "marginUsed", default)]
    pub margin_used: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct LeverageData {
    /// `cross` 或 `isolated`。
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct SpotClearinghouseState {
    #[serde(default)]
    pub balances: Vec<SpotBalance>,
    #[serde(rename = "tokenToAvailableAfterMaintenance", default)]
    pub token_to_available_after_maintenance: Vec<(u32, String)>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct SpotBalance {
    pub coin: String,
    #[serde(default)]
    pub token: Option<u32>,
    /// 余额项只在一项读不到时才用得上，缺字段就留空：解析失败会退回到
    /// 「读不出余额」这个明确的错误，而不是让整个响应解析不出来。
    #[serde(default)]
    pub total: String,
    #[serde(default)]
    pub hold: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct UserFees {
    /// 吃单费率。
    #[serde(rename = "userCrossRate")]
    pub user_cross_rate: String,
    /// 挂单费率。
    #[serde(rename = "userAddRate")]
    pub user_add_rate: String,
}

/// 已授权的 API 钱包。交易所可能在数组里放 `null`，跳过即可。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct AgentResponse {
    pub name: String,
    pub address: String,
    #[serde(rename = "validUntil")]
    pub valid_until: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct OpenOrderResponse {
    pub coin: String,
    pub oid: u64,
    #[serde(default)]
    pub cloid: Option<String>,
    /// `B` 买、`A` 卖。
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    /// 剩余数量。
    pub sz: String,
    /// 原始数量。缺了就只知道剩余量。
    #[serde(rename = "origSz", default)]
    pub orig_sz: Option<String>,
    #[serde(rename = "reduceOnly", default)]
    pub reduce_only: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct FillResponse {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    /// 成交时间，毫秒。
    pub time: i64,
    /// 成交标识，同一条成交在分页边界上可能重复出现，用它去重。
    pub tid: u64,
    /// 只有我们自己下的单才有 `cloid`；没有的成交不属于本策略。
    #[serde(default)]
    pub cloid: Option<String>,
}

/// `orderStatus` 里 `order` 的内容。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct OrderStatusResponse {
    pub order: OrderStatusOrder,
    /// `open` / `filled` / `canceled` / `rejected` / `marginCanceled` …
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct OrderStatusOrder {
    pub coin: String,
    pub oid: u64,
    #[serde(default)]
    pub cloid: Option<String>,
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    pub sz: String,
    #[serde(rename = "origSz", default)]
    pub orig_sz: Option<String>,
    #[serde(rename = "reduceOnly", default)]
    pub reduce_only: Option<bool>,
}

/// `activeAssetData` 里用得到的部分：这个标的上设置的杠杆。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ActiveAssetData {
    pub leverage: ActiveLeverage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct ActiveLeverage {
    /// `cross` 或 `isolated`。
    #[serde(rename = "type")]
    pub kind: String,
    pub value: u32,
}

/// `userFunding` 的一条：一次资金费结算。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct FundingEntry {
    pub delta: FundingDelta,
    /// 结算时刻（毫秒）。
    pub time: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct FundingDelta {
    pub coin: String,
    /// 这个账户收到的 USDC：负数是付出。文档样例里带前导空格。
    pub usdc: String,
}
