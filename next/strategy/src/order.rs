//! 订单身份、归属单位、用途和状态。
//!
//! 每张订单在发送前确定它属于哪个批次或格子、用途、限价和数量；客户端订单 ID 由
//! runtime 生成并持久化，永不复用。

use serde::{Deserialize, Serialize};

use crate::batch::BatchIndex;
use crate::grid::CellIndex;
use crate::units::{Price, Quantity};

/// 订单的归属单位：一批主仓或一个格子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Batch(BatchIndex),
    Cell(CellIndex),
}

impl std::fmt::Display for Unit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Batch(index) => write!(formatter, "batch {index}"),
            Self::Cell(index) => write!(formatter, "cell {index}"),
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

/// 订单用途：它记到哪个单位的哪一部分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderPurpose {
    /// 主仓买入：本批主仓部分，策略永不为它下平仓单。
    CoreBuy,
    /// 格子买入：买回待买回部分。
    GridBuy,
    /// 格子卖出：卖出持有部分。
    GridSell,
}

impl OrderPurpose {
    pub const fn side(self) -> Side {
        match self {
            Self::CoreBuy | Self::GridBuy => Side::Buy,
            Self::GridSell => Side::Sell,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CoreBuy => "core_buy",
            Self::GridBuy => "grid_buy",
            Self::GridSell => "grid_sell",
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
    /// 结果未知：超时、断连或含义不明的错误。
    Unknown,
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
