use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use grid_core::events::DomainEvent;
use grid_core::types::{Exposure, Side};

// ── Exchange types ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderReceipt {
    pub order_id: String,
    pub client_order_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeOrder {
    pub symbol: String,
    pub order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    pub realized_pnl: f64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub symbol: String,
    pub reference_price: f64,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeInfo {
    pub symbol: String,
    pub rules: grid_core::types::ExchangeRules,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserDataPayload {
    OrderUpdate(ExchangeOrder),
    PositionUpdate(Position),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserDataEvent {
    pub event_time: DateTime<Utc>,
    pub payload: UserDataPayload,
}

impl UserDataEvent {
    pub fn symbol(&self) -> &str {
        match &self.payload {
            UserDataPayload::OrderUpdate(order) => &order.symbol,
            UserDataPayload::PositionUpdate(position) => &position.symbol,
        }
    }
}

// ── Snapshot type (for persistence) ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Internal persistence snapshot: keeps full engine state, including fields not exposed over HTTP.
pub struct GridSnapshot {
    pub id: String,
    pub symbol: String,
    pub config: grid_core::strategy::GridConfig,
    pub status: super::instance::GridStatus,
    pub current_exposure: Exposure,
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<super::instance::PendingOrder>,
    pub risk_state: super::instance::RiskState,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedGridState {
    pub snapshot: GridSnapshot,
    pub events: Vec<DomainEvent>,
}

// ── Port traits ──

#[async_trait]
pub trait ExchangePort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, symbol: &str) -> Result<()>;
    async fn get_position(&self, symbol: &str) -> Result<Position>;
    async fn get_open_orders(&self, symbol: &str) -> Result<Vec<ExchangeOrder>>;
    async fn get_exchange_info(&self, symbol: &str) -> Result<ExchangeInfo>;
    async fn get_server_time(&self) -> Result<DateTime<Utc>>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, symbol: &str) -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait StateRepositoryPort: Send + Sync {
    async fn save_transition(
        &self,
        id: &str,
        state: &GridSnapshot,
        events: &[DomainEvent],
    ) -> Result<()>;
    async fn load_grid_state(&self, id: &str) -> Result<Option<GridSnapshot>>;
    async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
