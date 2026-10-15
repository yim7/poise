//! 交易所接口。
//!
//! 这是 runtime 唯一与外部世界打交道的出口。测试用模拟交易所实现，
//! Hyperliquid 适配属于后续任务。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Position, Price, Quantity};

/// 下单请求。客户端订单 ID 由 runtime 生成并先写入数据库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    pub instrument: Instrument,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub reduce_only: bool,
    /// 超过这个时间戳的动作会被交易所拒绝。
    pub expires_at: DateTime<Utc>,
}

/// 下单结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderReceipt {
    pub exchange_order_id: String,
    pub cumulative_filled: Quantity,
    pub status: OrderStatus,
}

/// 交易所上的一张挂单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeOrder {
    pub client_order_id: ClientOrderId,
    pub exchange_order_id: String,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub cumulative_filled: Quantity,
    pub reduce_only: bool,
    pub status: OrderStatus,
}

/// 按客户端订单 ID 查一张订单的结果。
///
/// 一张单完全成交后同样会从挂单列表里消失，所以「不在挂单列表里」不能直接当成
/// 撤销，必须问交易所这张单是什么结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderLookup {
    /// 交易所认识这张单，附上它现在的状态和累计成交量。
    Found(ExchangeOrder),
    /// 交易所查不到这张单（Hyperliquid 返回 `unknownOid`）。
    Unknown,
}

/// 一笔成交。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeFill {
    /// 交易所成交标识，用来去重。
    pub trade_id: String,
    pub client_order_id: ClientOrderId,
    pub price: Price,
    /// 本次成交数量。
    pub quantity: Quantity,
    /// 该订单的累计成交量。
    pub cumulative_quantity: Quantity,
    pub filled_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExchangeError {
    /// 交易所明确拒绝。
    #[error("exchange rejected the order: {reason}")]
    Rejected { reason: String },
    /// 交易所不认识这个客户端订单 ID。
    #[error("exchange does not know client order id `{client_order_id}`")]
    UnknownOrder { client_order_id: ClientOrderId },
    /// 结果未知：超时、断连或含义不明的错误。
    #[error("exchange result unknown: {reason}")]
    Unknown { reason: String },
    /// 暂时不可用。
    #[error("exchange unavailable: {reason}")]
    Unavailable { reason: String },
}

#[async_trait]
pub trait Exchange: Send + Sync {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError>;

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError>;

    /// 当前挂在交易所上的订单。
    async fn working_orders(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<ExchangeOrder>, ExchangeError>;

    /// 按客户端订单 ID 查一张订单现在的状态，含已经不在挂单列表里的订单。
    async fn order_status(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError>;

    /// 从 `since` 开始的成交记录，含边界（调用方按成交标识去重）。
    async fn fills_since(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<ExchangeFill>, ExchangeError>;

    async fn position(&self, instrument: &Instrument) -> Result<Position, ExchangeError>;

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError>;
}
