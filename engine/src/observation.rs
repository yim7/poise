use serde::{Deserialize, Serialize};

use crate::ports::{ExecutionQuote, OrderStatus};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MarketObservation {
    ExecutionQuote { execution_quote: ExecutionQuote },
    MarkPrice { mark_price: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn market_observation_has_explicit_variants_for_independent_market_inputs() {
        let _ = MarketObservation::ExecutionQuote {
            execution_quote: ExecutionQuote {
                best_bid: 99.9,
                best_ask: 100.1,
            },
        };
        let _ = MarketObservation::MarkPrice { mark_price: 100.0 };
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionObservation {
    pub qty: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderObservation {
    pub order_id: String,
    pub client_order_id: String,
    pub side: poise_core::types::Side,
    pub price: f64,
    pub quantity: f64,
    #[serde(default)]
    pub filled_qty: f64,
    pub realized_pnl: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompleteOpenOrderSnapshot {
    orders: Vec<OrderObservation>,
}

impl CompleteOpenOrderSnapshot {
    /// Build only from a complete exchange open-orders query result.
    /// Missing local working bindings are treated as absent from the exchange.
    pub fn from_complete_exchange_query(orders: Vec<OrderObservation>) -> Self {
        Self { orders }
    }

    pub fn orders(&self) -> &[OrderObservation] {
        &self.orders
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrackObservation {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(OrderObservation),
}
