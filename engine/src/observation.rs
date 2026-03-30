use serde::{Deserialize, Serialize};

use crate::ports::OrderStatus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketObservation {
    pub reference_price: f64,
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
    pub realized_pnl: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GridObservation {
    Market(MarketObservation),
    Position(PositionObservation),
    Order(OrderObservation),
}
