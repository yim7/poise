use poise_core::track::Instrument;
use poise_core::types::Side;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub instrument: Instrument,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
    #[serde(default)]
    pub reduce_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderReceipt {
    pub order_id: String,
    pub client_order_id: String,
    #[serde(default)]
    pub filled_qty: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeOrder {
    pub instrument: Instrument,
    pub order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    #[serde(default)]
    pub filled_qty: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Submitting,
    New,
    PartiallyFilled,
    Filled,
    Canceling,
    Canceled,
    Rejected,
    Expired,
}

impl OrderStatus {
    pub(crate) fn effective_cumulative_filled_qty(
        self,
        reported_filled_qty: f64,
        order_quantity: f64,
    ) -> f64 {
        if self == Self::Filled && reported_filled_qty <= f64::EPSILON {
            return order_quantity;
        }
        reported_filled_qty.max(0.0)
    }

    pub fn keeps_working_order(self) -> bool {
        matches!(
            self,
            Self::Submitting | Self::New | Self::PartiallyFilled | Self::Canceling
        )
    }

    pub fn clears_working_order(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Rejected | Self::Expired
        )
    }

    pub fn should_reconcile_after_order_update(self) -> bool {
        matches!(self, Self::Canceled | Self::Rejected | Self::Expired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutionQuote {
    pub best_bid: f64,
    pub best_ask: f64,
}

#[cfg(test)]
mod tests {
    use super::OrderStatus;

    #[test]
    fn filled_status_without_reported_quantity_means_full_order_quantity() {
        assert_eq!(
            OrderStatus::Filled.effective_cumulative_filled_qty(0.0, 1.25),
            1.25
        );
        assert_eq!(
            OrderStatus::PartiallyFilled.effective_cumulative_filled_qty(0.5, 1.25),
            0.5
        );
    }
}
