use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use poise_core::instrument::Amount;

#[derive(Debug, Clone, PartialEq)]
pub struct StoredAccountMonitorState {
    pub trading_day: NaiveDate,
    pub baseline_equity: Amount,
    pub baseline_captured_at: DateTime<Utc>,
}

#[async_trait]
pub trait AccountMonitorStore: Send + Sync {
    async fn load_state(&self) -> Result<Option<StoredAccountMonitorState>>;
    async fn save_state(&self, state: &StoredAccountMonitorState) -> Result<()>;
}
