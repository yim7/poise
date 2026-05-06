use std::{fmt, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use poise_core::types::Side;

use crate::ledger::TrackPnlRecord;
use poise_core::track::Instrument;

// ── Exchange types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPortErrorKind {
    Failed,
    CancelOutcomeUnknown,
    InsufficientMargin,
    InvalidPriceIncrement,
    RateLimited,
}

impl fmt::Display for ExecutionPortErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed => write!(f, "execution failed"),
            Self::CancelOutcomeUnknown => write!(f, "cancel outcome unknown"),
            Self::InsufficientMargin => write!(f, "insufficient margin"),
            Self::InvalidPriceIncrement => write!(f, "invalid price increment"),
            Self::RateLimited => write!(f, "rate limited"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{kind}: {source}")]
pub struct ExecutionPortError {
    kind: ExecutionPortErrorKind,
    #[source]
    source: anyhow::Error,
}

impl ExecutionPortError {
    pub fn new(kind: ExecutionPortErrorKind, source: anyhow::Error) -> Self {
        Self { kind, source }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(
            ExecutionPortErrorKind::Failed,
            anyhow::Error::msg(message.into()),
        )
    }

    pub fn kind(&self) -> ExecutionPortErrorKind {
        self.kind
    }
}

impl From<anyhow::Error> for ExecutionPortError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(ExecutionPortErrorKind::Failed, error)
    }
}

pub type ExecutionResult<T> = std::result::Result<T, ExecutionPortError>;

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
    #[serde(default)]
    pub filled_qty: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangeOpenOrderSnapshot {
    orders: Vec<ExchangeOrder>,
}

impl ExchangeOpenOrderSnapshot {
    /// Build only from a complete exchange open-orders query result.
    /// Missing orders are interpreted as absent from the exchange.
    pub fn from_complete_exchange_query(orders: Vec<ExchangeOrder>) -> Self {
        Self { orders }
    }

    pub fn orders(&self) -> &[ExchangeOrder] {
        &self.orders
    }

    pub fn into_orders(self) -> Vec<ExchangeOrder> {
        self.orders
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutionQuote {
    pub best_bid: f64,
    pub best_ask: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionQuoteTick {
    pub instrument: Instrument,
    pub execution_quote: ExecutionQuote,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkPriceTick {
    pub instrument: Instrument,
    pub mark_price: f64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MarketDataTick {
    ExecutionQuote(ExecutionQuoteTick),
    MarkPrice(MarkPriceTick),
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
    TrackPnl(TrackPnlRecord),
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
            UserDataPayload::TrackPnl(record) => &record.instrument,
        }
    }
}

// ── Port traits ──

#[async_trait]
pub trait AccountSummaryPort: Send + Sync {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot>;

    async fn get_available_balance(&self, _instrument: &Instrument) -> Result<f64> {
        Ok(self.get_account_summary().await?.available)
    }
}

#[async_trait]
pub trait ExecutionPort: Send + Sync {
    async fn submit_order(&self, req: OrderRequest) -> ExecutionResult<OrderReceipt>;
    async fn cancel_order(
        &self,
        instrument: &Instrument,
        order_id: &str,
    ) -> ExecutionResult<OrderReceipt>;
    async fn cancel_all(&self, instrument: &Instrument) -> ExecutionResult<()>;
    async fn get_position(&self, instrument: &Instrument) -> ExecutionResult<Position>;
    async fn get_open_orders(
        &self,
        instrument: &Instrument,
    ) -> ExecutionResult<ExchangeOpenOrderSnapshot>;
}

#[async_trait]
pub trait AccountPort: Send + Sync {
    async fn get_account_capacity_snapshot(
        &self,
        instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot>;
    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait MetadataPort: Send + Sync {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo>;
    async fn get_server_time(&self) -> Result<DateTime<Utc>>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>>;
}

#[derive(Clone)]
pub struct ExchangePorts {
    execution: Arc<dyn ExecutionPort>,
    market_data: Arc<dyn MarketDataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    account: Arc<dyn AccountPort>,
    metadata: Arc<dyn MetadataPort>,
}

impl ExchangePorts {
    pub fn new(
        execution: Arc<dyn ExecutionPort>,
        market_data: Arc<dyn MarketDataPort>,
        account_summary: Arc<dyn AccountSummaryPort>,
        account: Arc<dyn AccountPort>,
        metadata: Arc<dyn MetadataPort>,
    ) -> Self {
        Self {
            execution,
            market_data,
            account_summary,
            account,
            metadata,
        }
    }

    pub fn execution(&self) -> Arc<dyn ExecutionPort> {
        Arc::clone(&self.execution)
    }

    pub fn market_data(&self) -> Arc<dyn MarketDataPort> {
        Arc::clone(&self.market_data)
    }

    pub fn account_summary(&self) -> Arc<dyn AccountSummaryPort> {
        Arc::clone(&self.account_summary)
    }

    pub fn account(&self) -> Arc<dyn AccountPort> {
        Arc::clone(&self.account)
    }

    pub fn metadata(&self) -> Arc<dyn MetadataPort> {
        Arc::clone(&self.metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_result_uses_execution_port_error() {
        fn assert_execution_result<T>(result: ExecutionResult<T>) -> ExecutionResult<T> {
            result
        }

        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::CancelOutcomeUnknown,
            anyhow::anyhow!("Unknown order sent."),
        );

        let result = assert_execution_result::<OrderReceipt>(Err(error));

        assert_eq!(
            result.unwrap_err().kind(),
            ExecutionPortErrorKind::CancelOutcomeUnknown
        );
    }

    #[test]
    fn execution_port_error_preserves_source_error() {
        let error = ExecutionPortError::from(anyhow::anyhow!("raw exchange failure"));

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(source.to_string(), "raw exchange failure");
    }

    #[test]
    fn cancel_outcome_unknown_preserves_source_error() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::CancelOutcomeUnknown,
            anyhow::anyhow!("Unknown order sent."),
        );

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(source.to_string(), "Unknown order sent.");
        assert_eq!(error.kind(), ExecutionPortErrorKind::CancelOutcomeUnknown);
    }

    #[test]
    fn insufficient_margin_preserves_source_error() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::InsufficientMargin,
            anyhow::anyhow!("Margin is insufficient."),
        );

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(source.to_string(), "Margin is insufficient.");
        assert_eq!(error.kind(), ExecutionPortErrorKind::InsufficientMargin);
    }

    #[test]
    fn invalid_price_increment_preserves_source_error() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::InvalidPriceIncrement,
            anyhow::anyhow!("Price must be divisible by tick size."),
        );

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(source.to_string(), "Price must be divisible by tick size.");
        assert_eq!(error.kind(), ExecutionPortErrorKind::InvalidPriceIncrement);
    }

    #[test]
    fn rate_limited_preserves_source_error() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::RateLimited,
            anyhow::anyhow!("too many requests"),
        );

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(source.to_string(), "too many requests");
        assert_eq!(error.kind(), ExecutionPortErrorKind::RateLimited);
    }

    #[test]
    fn wraps_source_error_with_kind() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::InvalidPriceIncrement,
            anyhow::anyhow!("exchange tick rejection"),
        );

        let source = std::error::Error::source(&error).expect("source error should be preserved");

        assert_eq!(error.kind(), ExecutionPortErrorKind::InvalidPriceIncrement);
        assert_eq!(source.to_string(), "exchange tick rejection");
    }
}

pub trait ClockPort: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
