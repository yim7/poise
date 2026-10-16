//! 订单身份、归属单位、用途和状态。
//!
//! 每张订单在发送前确定它属于哪个批次或格子、用途、限价和数量；客户端订单 ID 由
//! runtime 生成并持久化，永不复用。

use serde::{Deserialize, Serialize};

use crate::batch::BatchIndex;
use crate::grid::CellIndex;
use crate::side::Direction;
use crate::units::{Price, Quantity};

/// 订单的归属单位：某一侧的一批主仓或一个格子。
///
/// 中性网格两侧都有批次和格子，所以要带上方向才能区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Batch(Direction, BatchIndex),
    Cell(Direction, CellIndex),
    /// 整个一侧：观点失效时的减仓单不属于哪一批、哪一格，成交按「先网格、后主仓」
    /// 记到这一侧上（第 7.5 节）。
    Side(Direction),
}

impl Unit {
    /// 这个单位属于哪一侧。
    pub const fn direction(self) -> Direction {
        match self {
            Self::Batch(direction, _) | Self::Cell(direction, _) | Self::Side(direction) => {
                direction
            }
        }
    }

    /// 这个单位在启动顺序上的下标：批次的顺序就是批次下标，格子的顺序就是格子下标
    /// （两者都是从靠近空隙的一端开始）。整个一侧不参与启动顺序，是 0。
    pub const fn index(self) -> usize {
        match self {
            Self::Batch(_, index) => index.0,
            Self::Cell(_, index) => index.0,
            Self::Side(_) => 0,
        }
    }

    /// 这个单位的这一部分在这一侧上的订单方向。
    pub const fn side(self, purpose: OrderPurpose) -> Side {
        purpose.side(self.direction())
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Batch(direction, index) => write!(formatter, "{direction} batch {index}"),
            Self::Cell(direction, index) => write!(formatter, "{direction} cell {index}"),
            Self::Side(direction) => write!(formatter, "{direction} side"),
        }
    }
}

/// 客户端订单 ID（Hyperliquid `cloid`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientOrderId(String);

impl ClientOrderId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, OrderError> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(OrderError::EmptyClientOrderId);
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientOrderId {
    type Error = OrderError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<ClientOrderId> for String {
    fn from(value: ClientOrderId) -> String {
        value.0
    }
}

impl std::fmt::Display for ClientOrderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrderError {
    #[error("client order id must not be empty")]
    EmptyClientOrderId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// 订单用途：它记到哪个单位的哪一部分，也就是订单的腿。
///
/// 建仓腿与平仓腿在各侧上的买卖方向相反（设计第 5.1 节），所以这里只记「哪条腿」，
/// 具体买卖由 [`OrderPurpose::side`] 按方向算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderPurpose {
    /// 主仓建仓腿：本批主仓部分，策略永不为它下平仓单。
    Core,
    /// 格子的建仓腿：多头侧买入，空头侧卖出开空。
    GridEntry,
    /// 格子的平仓腿：多头侧卖出，空头侧买入平仓。
    GridExit,
    /// 观点失效后的减仓单：只减仓、立即成交否则撤销，方向和平仓腿相同（第 7.5 节）。
    Invalidation,
}

impl OrderPurpose {
    /// 这条腿在某一侧上的订单方向。
    pub const fn side(self, direction: Direction) -> Side {
        match self {
            Self::Core | Self::GridEntry => direction.entry_side(),
            Self::GridExit | Self::Invalidation => direction.exit_side(),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::GridEntry => "grid_entry",
            Self::GridExit => "grid_exit",
            Self::Invalidation => "invalidation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// 已写入数据库、尚未收到交易所回应。
    Submitting,
    Open,
    PartiallyFilled,
    Filled,
    Canceled,
    /// 交易所明确拒绝。
    Rejected,
    /// 结果未知：超时、断连或含义不明的错误，或者交易所查不到它（`unknownOid`）。
    Unknown,
    /// 作废：交易所查不到它，期限过后核对挂单、成交、仓位三项一致，判定它没有任何影响，
    /// 释放预留（第 9.2 节第 3 条）。
    Void,
}

impl OrderStatus {
    /// 这张订单是否还可能在交易所成交。
    pub const fn is_working(self) -> bool {
        matches!(
            self,
            Self::Submitting | Self::Open | Self::PartiallyFilled | Self::Unknown
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submitting => "submitting",
            Self::Open => "open",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
            Self::Void => "void",
        }
    }
}

/// 一张订单的领域记录：身份、归属、用途和已成交量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub client_order_id: ClientOrderId,
    pub unit: Unit,
    pub purpose: OrderPurpose,
    pub limit_price: Price,
    pub quantity: Quantity,
    pub cumulative_filled: Quantity,
    pub reduce_only: bool,
    pub status: OrderStatus,
}

impl OrderRecord {
    /// 尚未成交的数量。
    pub fn remaining(&self) -> Quantity {
        self.quantity.saturating_sub(self.cumulative_filled)
    }

    pub fn is_working(&self) -> bool {
        self.status.is_working()
    }

    /// 规划时使用的在场订单视图。
    pub fn working_view(&self) -> WorkingOrder {
        WorkingOrder {
            client_order_id: self.client_order_id.clone(),
            unit: self.unit,
            purpose: self.purpose,
            limit_price: self.limit_price,
            remaining: self.remaining(),
        }
    }
}

/// 规划输入中的在场订单：只保留判断所需的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingOrder {
    pub client_order_id: ClientOrderId,
    pub unit: Unit,
    pub purpose: OrderPurpose,
    pub limit_price: Price,
    pub remaining: Quantity,
}

#[cfg(test)]
mod tests {
    use super::{ClientOrderId, OrderError, OrderStatus};

    #[test]
    fn client_order_id_rejects_empty_values() {
        assert_eq!(
            ClientOrderId::try_new("  ").unwrap_err(),
            OrderError::EmptyClientOrderId
        );
        assert_eq!(ClientOrderId::try_new("0x01").unwrap().as_str(), "0x01");
    }

    #[test]
    fn working_statuses_keep_the_reservation() {
        assert!(OrderStatus::Submitting.is_working());
        assert!(OrderStatus::Open.is_working());
        assert!(OrderStatus::PartiallyFilled.is_working());
        assert!(OrderStatus::Unknown.is_working());
        assert!(!OrderStatus::Filled.is_working());
        assert!(!OrderStatus::Canceled.is_working());
        assert!(!OrderStatus::Rejected.is_working());
    }
}
