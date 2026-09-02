use std::{collections::BTreeMap, fmt, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_core::instrument::{Amount, Asset};
use poise_core::track::Instrument;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub use poise_engine::exchange::{
    ExchangeOrder, ExecutionQuote, OrderReceipt, OrderRequest, OrderStatus,
};
pub use poise_engine::ledger::{TrackPnlRecord, TrackPnlRecordKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPortErrorKind {
    Failed,
    SubmitOutcomeUnknown,
    CancelOutcomeUnknown,
    InsufficientMargin,
    InvalidPriceIncrement,
    RateLimited,
}

impl fmt::Display for ExecutionPortErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed => write!(f, "execution failed"),
            Self::SubmitOutcomeUnknown => write!(f, "submit outcome unknown"),
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
pub struct Position {
    pub instrument: Instrument,
    pub qty: f64,
    pub avg_price: f64,
    pub unrealized_pnl: f64,
    pub mark_price: Option<f64>,
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
    pub instrument_spec: poise_core::instrument::InstrumentSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountCapacitySnapshot {
    pub max_increase_notional: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSummarySnapshot {
    equity: Amount,
    available: Option<Amount>,
    #[serde(default)]
    available_by_asset: BTreeMap<Asset, f64>,
    unrealized_pnl: Option<Amount>,
    observed_at: DateTime<Utc>,
}

impl AccountSummarySnapshot {
    pub fn try_new(
        equity: Amount,
        available: Option<Amount>,
        available_by_asset: BTreeMap<Asset, f64>,
        unrealized_pnl: Option<Amount>,
        observed_at: DateTime<Utc>,
    ) -> Result<Self, String> {
        for amount in [available.as_ref(), unrealized_pnl.as_ref()]
            .into_iter()
            .flatten()
        {
            if amount.asset() != equity.asset() {
                return Err(format!(
                    "account amount asset `{}` does not match valuation asset `{}`",
                    amount.asset().as_str(),
                    equity.asset().as_str()
                ));
            }
        }
        for (asset, value) in &available_by_asset {
            if !value.is_finite() {
                return Err(format!(
                    "available balance for `{}` must be finite, got {value}",
                    asset.as_str()
                ));
            }
        }

        Ok(Self {
            equity,
            available,
            available_by_asset,
            unrealized_pnl,
            observed_at,
        })
    }

    pub fn valuation_asset(&self) -> &Asset {
        self.equity.asset()
    }

    pub fn equity(&self) -> &Amount {
        &self.equity
    }

    pub fn available(&self) -> Option<&Amount> {
        self.available.as_ref()
    }

    pub fn available_by_asset(&self) -> &BTreeMap<Asset, f64> {
        &self.available_by_asset
    }

    pub fn unrealized_pnl(&self) -> Option<&Amount> {
        self.unrealized_pnl.as_ref()
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub fn with_equity_and_available(
        mut self,
        equity: Amount,
        available: Amount,
    ) -> Result<Self, String> {
        if equity.asset() != self.valuation_asset() || available.asset() != self.valuation_asset() {
            return Err(format!(
                "replacement account amounts must use valuation asset `{}`",
                self.valuation_asset().as_str()
            ));
        }
        self.equity = equity;
        self.available = Some(available);
        Ok(self)
    }

    pub fn available_for_asset(&self, asset: &str) -> Option<f64> {
        self.available_by_asset
            .iter()
            .find_map(|(candidate, value)| (candidate.as_str() == asset).then_some(*value))
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnlHistoryWindow {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl PnlHistoryWindow {
    pub fn try_new(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Self, String> {
        if start > end {
            return Err(format!(
                "PNL history window start `{start}` is after end `{end}`"
            ));
        }
        Ok(Self { start, end })
    }

    pub fn covers(&self, required: &Self) -> bool {
        self.start <= required.start && self.end >= required.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecentPnlCompleteness {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecentTrackPnlBatch {
    pub records: Vec<TrackPnlRecord>,
    pub covered_window: Option<PnlHistoryWindow>,
    pub completeness: RecentPnlCompleteness,
}

impl RecentTrackPnlBatch {
    pub fn new(
        records: Vec<TrackPnlRecord>,
        covered_window: Option<PnlHistoryWindow>,
        completeness: RecentPnlCompleteness,
    ) -> Self {
        Self {
            records,
            covered_window,
            completeness,
        }
    }

    pub fn complete(covered_window: PnlHistoryWindow, records: Vec<TrackPnlRecord>) -> Self {
        Self::new(
            records,
            Some(covered_window),
            RecentPnlCompleteness::Complete,
        )
    }

    pub fn incomplete(
        covered_window: Option<PnlHistoryWindow>,
        records: Vec<TrackPnlRecord>,
    ) -> Self {
        Self::new(records, covered_window, RecentPnlCompleteness::Incomplete)
    }

    pub fn covers(&self, required_window: &PnlHistoryWindow) -> bool {
        self.completeness == RecentPnlCompleteness::Complete
            && self
                .covered_window
                .as_ref()
                .is_some_and(|covered| covered.covers(required_window))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecentTrackPnlRecords {
    Available(RecentTrackPnlBatch),
    Unsupported,
}

#[async_trait]
pub trait AccountSummaryPort: Send + Sync {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot>;

    async fn get_available_balance(&self, _instrument: &Instrument) -> Result<f64> {
        let summary = self.get_account_summary().await?;
        Ok(summary
            .available()
            .context("account-wide available amount is unavailable")?
            .value())
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

    async fn get_recent_track_pnl_records(
        &self,
        instrument: &Instrument,
        required_window: &PnlHistoryWindow,
    ) -> Result<RecentTrackPnlRecords>;

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>>;
}

#[async_trait]
pub trait MetadataPort: Send + Sync {
    async fn get_exchange_info(&self, instrument: &Instrument) -> Result<ExchangeInfo>;

    async fn get_server_time(&self) -> Result<chrono::DateTime<chrono::Utc>>;
}

#[async_trait]
pub trait MarketDataPort: Send + Sync {
    async fn subscribe_prices(
        &self,
        instrument: &Instrument,
    ) -> Result<mpsc::Receiver<MarketDataTick>>;

    async fn get_mark_price(&self, _instrument: &Instrument) -> Result<Option<f64>> {
        Ok(None)
    }
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
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use poise_core::instrument::{Amount, Asset};

    use super::{
        AccountSummarySnapshot, ExecutionPortError, ExecutionPortErrorKind, PnlHistoryWindow,
        RecentPnlCompleteness, RecentTrackPnlBatch,
    };

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    #[test]
    fn account_summary_rejects_mixed_valuation_assets() {
        let error = AccountSummarySnapshot::try_new(
            amount(10_000.0, "USD"),
            Some(amount(8_000.0, "USDT")),
            BTreeMap::new(),
            Some(amount(-50.0, "USD")),
            Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
        )
        .unwrap_err();

        assert!(error.contains("USDT"));
        assert!(error.contains("USD"));
    }

    #[test]
    fn account_summary_keeps_per_asset_balances_separate() {
        let snapshot = AccountSummarySnapshot::try_new(
            amount(20_000.0, "USD"),
            None,
            BTreeMap::from([
                (Asset::try_new("BTC").unwrap(), 0.1),
                (Asset::try_new("USDT").unwrap(), 5_000.0),
            ]),
            None,
            Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
        )
        .unwrap();

        assert_eq!(snapshot.available_for_asset("BTC"), Some(0.1));
        assert_eq!(snapshot.available_for_asset("USDT"), Some(5_000.0));
    }

    #[test]
    fn complete_recent_pnl_batch_must_cover_the_required_window() {
        let required = PnlHistoryWindow::try_new(
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
        )
        .unwrap();
        let partial = PnlHistoryWindow::try_new(
            Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
        )
        .unwrap();

        assert!(RecentTrackPnlBatch::complete(required.clone(), Vec::new()).covers(&required));
        assert!(
            !RecentTrackPnlBatch::new(Vec::new(), Some(partial), RecentPnlCompleteness::Complete,)
                .covers(&required)
        );
    }

    #[test]
    fn pnl_history_window_rejects_an_inverted_range() {
        let error = PnlHistoryWindow::try_new(
            Utc.with_ymd_and_hms(2026, 9, 2, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
        )
        .unwrap_err();

        assert!(error.contains("start"));
    }

    #[test]
    fn execution_port_error_preserves_kind_and_source() {
        let error = ExecutionPortError::new(
            ExecutionPortErrorKind::SubmitOutcomeUnknown,
            anyhow::anyhow!("response ended before acknowledgement"),
        );

        assert_eq!(error.kind(), ExecutionPortErrorKind::SubmitOutcomeUnknown);
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "response ended before acknowledgement"
        );
    }
}
