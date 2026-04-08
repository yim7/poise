use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use poise_core::types::Side;

use crate::ledger::TrackLedgerEvent;
use crate::track::Instrument;

// ── Exchange types ──

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
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub instrument: Instrument,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeOrder {
    pub instrument: Instrument,
    pub order_id: String,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub qty: f64,
    pub realized_pnl: f64,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTick {
    pub instrument: Instrument,
    pub reference_price: f64,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeInfo {
    pub instrument: Instrument,
    pub rules: poise_core::types::ExchangeRules,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacitySnapshot {
    pub max_increase_notional: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSummarySnapshot {
    pub equity: f64,
    pub available: f64,
    pub unrealized_pnl: f64,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserDataPayload {
    OrderUpdate(ExchangeOrder),
    PositionUpdate(Position),
    TrackLedger(TrackLedgerUpdate),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackLedgerUpdate {
    pub instrument: Instrument,
    pub event: TrackLedgerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserDataEvent {
    pub event_time: DateTime<Utc>,
    pub payload: UserDataPayload,
}

impl UserDataEvent {
    pub fn instrument(&self) -> &Instrument {
        match &self.payload {
            UserDataPayload::OrderUpdate(order) => &order.instrument,
            UserDataPayload::PositionUpdate(position) => &position.instrument,
            UserDataPayload::TrackLedger(update) => &update.instrument,
        }
    }
}

// ── Port traits ──

#[async_trait]
pub trait AccountSummaryPort: Send + Sync {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot>;
}

#[async_trait]
pub trait ExchangePort: AccountSummaryPort + Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, instrument: &Instrument) -> Result<()>;
    async fn get_position(&self, instrument: &Instrument) -> Result<Position>;
    async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>>;
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo>;
    async fn get_account_capacity_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot>;
    async fn get_server_time(&self) -> Result<DateTime<Utc>>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
