use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use grid_core::events::DomainEvent;
use grid_core::types::Side;

use crate::grid::{GridId, Instrument};
use crate::snapshot::GridRuntimeSnapshot;
use crate::transition::GridEffect;

pub use crate::snapshot::GridRuntimeSnapshot as GridSnapshot;

// ── Exchange types ──

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub instrument: Instrument,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub client_order_id: String,
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
    pub fn instrument(&self) -> &Instrument {
        match &self.payload {
            UserDataPayload::OrderUpdate(order) => &order.instrument,
            UserDataPayload::PositionUpdate(position) => &position.instrument,
        }
    }
}

// ── Port traits ──

#[async_trait]
pub trait ExchangePort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt>;
    async fn cancel_order(&self, instrument: &Instrument, order_id: &str) -> Result<()>;
    async fn cancel_all(&self, instrument: &Instrument) -> Result<()>;
    async fn get_position(&self, instrument: &Instrument) -> Result<Position>;
    async fn get_open_orders(&self, instrument: &Instrument) -> Result<Vec<ExchangeOrder>>;
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo>;
    async fn get_server_time(&self) -> Result<DateTime<Utc>>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait StateRepositoryPort: Send + Sync {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &GridRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[GridEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedGridWrite>;

    async fn save_transition(
        &self,
        id: &str,
        state: &GridRuntimeSnapshot,
        events: &[DomainEvent],
        effects: &[GridEffect],
    ) -> Result<CommittedGridWrite> {
        self.save_transition_with_effect_status(id, state, events, effects, None)
            .await
    }
    async fn load_grid_state(&self, id: &str) -> Result<Option<GridRuntimeSnapshot>>;
    async fn list_events(&self, id: &str) -> Result<Vec<DomainEvent>>;
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedGridEffect>>;
    async fn list_pending_submit_effects_for_grid(
        &self,
        grid_id: &GridId,
    ) -> Result<Vec<PersistedGridEffect>>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDomainEvent {
    pub id: i64,
    pub grid_id: GridId,
    pub event: DomainEvent,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredGridSnapshot {
    pub snapshot: GridRuntimeSnapshot,
    pub updated_at: DateTime<Utc>,
}

#[async_trait]
pub trait GridReadRepositoryPort: Send + Sync {
    async fn list_grid_snapshots(&self) -> Result<Vec<StoredGridSnapshot>>;
    async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<StoredGridSnapshot>>;
    async fn list_recent_grid_events(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<StoredDomainEvent>>;
    /// Returns effects selected from the most recent `updated_at` window,
    /// ordered by `updated_at` ascending.
    async fn list_recent_grid_effects(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<PersistedGridEffect>>;
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommittedGridWrite {
    pub grid_id: GridId,
    pub effects: Vec<PersistedGridEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectStatusUpdate {
    pub effect_id: String,
    pub status: EffectStatus,
    pub attempt_delta: u32,
    pub last_error: Option<String>,
}

impl EffectStatusUpdate {
    pub fn succeeded(effect_id: impl Into<String>) -> Self {
        Self {
            effect_id: effect_id.into(),
            status: EffectStatus::Succeeded,
            attempt_delta: 0,
            last_error: None,
        }
    }

    pub fn superseded(effect_id: impl Into<String>) -> Self {
        Self {
            effect_id: effect_id.into(),
            status: EffectStatus::Superseded,
            attempt_delta: 0,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedGridEffect {
    pub effect_id: String,
    pub grid_id: GridId,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: GridEffect,
    pub status: EffectStatus,
    pub attempt_count: u32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Pending,
    Executing,
    Succeeded,
    Superseded,
    Failed,
}

impl EffectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }

    pub fn unblocks_follow_up(self) -> bool {
        matches!(self, Self::Succeeded | Self::Superseded)
    }
}
