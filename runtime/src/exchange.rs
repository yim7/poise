//! 交易所接口。
//!
//! 这是 runtime 唯一与外部世界打交道的出口。测试用模拟交易所实现，
//! 真实交易所的实现在 `poise-hyperliquid`。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Notional, Position, Price, Quantity};
use serde::{Deserialize, Serialize};

/// 下单请求。客户端订单 ID 由 runtime 生成并先写入数据库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    pub instrument: Instrument,
    pub client_order_id: ClientOrderId,
    pub side: Side,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub reduce_only: bool,
    pub time_in_force: TimeInForce,
    /// 超过这个时间戳的动作会被交易所拒绝。
    pub expires_at: DateTime<Utc>,
}

/// 交易所侧的止损触发单（兜底单）：标记价越过触发价时交易所自己按市价减仓，poise
/// 掉线时也会执行。只减仓。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRequest {
    pub instrument: Instrument,
    pub client_order_id: ClientOrderId,
    /// 减仓的方向：多仓卖、空仓买。
    pub side: Side,
    /// 标记价越过它时触发。
    pub trigger_price: Price,
    /// 触发后的市价单最差成交价。
    pub limit_price: Price,
    pub quantity: Quantity,
    /// 超过这个时间戳的动作会被交易所拒绝。
    pub expires_at: DateTime<Utc>,
}

/// 订单的有效期策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    /// 一直挂到成交或撤销：网格和主仓的挂单。
    GoodTillCanceled,
    /// 能成交的部分立即成交，剩下的立即撤销：观点失效的减仓单（第 7.5 节）。
    ImmediateOrCancel,
}

/// 下单结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderReceipt {
    pub exchange_order_id: String,
    pub cumulative_filled: Quantity,
    pub status: OrderStatus,
}

/// 交易所上的一张挂单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeOrder {
    /// 客户端订单 ID。用户手动下的单没有它：这样的单不属于任何策略，只在预览里列出
    /// （第 8 节第 4 条），对账时一律忽略。
    pub client_order_id: Option<ClientOrderId>,
    pub exchange_order_id: String,
    pub side: Side,
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
    /// 这笔成交实际付的手续费（负数是返佣）；交易所没给时是 `None`，用的地方按费率估算。
    pub fee: Option<Notional>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExchangeError {
    /// 交易所明确拒绝。
    #[error("交易所拒单：{reason}")]
    Rejected { reason: String },
    /// 交易所以保证金不足拒绝下单（第 10 节）：和 [`ExchangeError::Rejected`] 一样是明确
    /// 拒绝，单独列出来，调用方据此退避重试，不要每一步都再撞一次。
    #[error("保证金不足被拒：{reason}")]
    InsufficientMargin { reason: String },
    /// 交易所以操作次数超限拒绝（第 10 节，B30）：Unified 账户每天的用户操作上限，或者
    /// 按地址的累计请求上限。和明确拒绝一样没有落地；单独列出来，调用方据此暂停策略。
    #[error("请求次数超限被拒：{reason}")]
    RateLimited { reason: String },
    /// 交易所不认识这个客户端订单 ID。
    #[error("交易所查无此单 {client_order_id}")]
    UnknownOrder { client_order_id: ClientOrderId },
    /// 结果未知：超时、断连或含义不明的错误。
    #[error("结果未知：{reason}")]
    Unknown { reason: String },
    /// 暂时不可用。
    #[error("交易所暂不可用：{reason}")]
    Unavailable { reason: String },
}

#[async_trait]
pub trait Exchange: Send + Sync {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError>;

    /// 挂一张交易所侧的止损触发单（[`StopRequest`]）。撤销和查询与普通订单一样按客户端
    /// 订单 ID。
    async fn place_stop(&self, request: &StopRequest) -> Result<OrderReceipt, ExchangeError>;

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

    /// 几个标的的净仓位，顺序和 `instruments` 一致。默认逐个读；能按市场一次读完的交易所
    /// 覆盖它（第 7.7 节第 8 条）。
    async fn positions(
        &self,
        instruments: &[Instrument],
    ) -> Result<Vec<(Instrument, Position)>, ExchangeError> {
        let mut positions = Vec::with_capacity(instruments.len());
        for instrument in instruments {
            positions.push((instrument.clone(), self.position(instrument).await?));
        }
        Ok(positions)
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError>;
}
