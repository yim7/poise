use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use poise_engine::ports::AccountSummarySnapshot;
use poise_storage::sqlite::{
    AccountMonitorObservedSnapshotRow, AccountMonitorStateRow, SqliteStorage,
};

#[derive(Debug, Clone, PartialEq)]
pub struct StoredAccountMonitorState {
    pub trading_day: NaiveDate,
    pub baseline_equity: f64,
    pub baseline_captured_at: DateTime<Utc>,
    pub last_observed_account_snapshot: Option<AccountSummarySnapshot>,
}

#[async_trait]
pub trait AccountMonitorStore: Send + Sync {
    async fn load_state(&self) -> Result<Option<StoredAccountMonitorState>>;
    async fn save_state(&self, state: &StoredAccountMonitorState) -> Result<()>;
}

pub struct SqliteAccountMonitorStore {
    storage: Arc<SqliteStorage>,
}

impl SqliteAccountMonitorStore {
    pub fn new(storage: Arc<SqliteStorage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl AccountMonitorStore for SqliteAccountMonitorStore {
    async fn load_state(&self) -> Result<Option<StoredAccountMonitorState>> {
        Ok(self
            .storage
            .load_account_monitor_state_row()
            .await?
            .map(|row| StoredAccountMonitorState {
                trading_day: row.trading_day,
                baseline_equity: row.baseline_equity,
                baseline_captured_at: row.baseline_captured_at,
                last_observed_account_snapshot: row.last_observed_snapshot.map(|snapshot| {
                    AccountSummarySnapshot {
                        equity: snapshot.equity,
                        available: snapshot.available,
                        unrealized_pnl: snapshot.unrealized_pnl,
                        observed_at: snapshot.observed_at,
                    }
                }),
            }))
    }

    async fn save_state(&self, state: &StoredAccountMonitorState) -> Result<()> {
        let row = AccountMonitorStateRow {
            trading_day: state.trading_day,
            baseline_equity: state.baseline_equity,
            baseline_captured_at: state.baseline_captured_at,
            last_observed_snapshot: state
                .last_observed_account_snapshot
                .as_ref()
                .map(|snapshot| AccountMonitorObservedSnapshotRow {
                    equity: snapshot.equity,
                    available: snapshot.available,
                    unrealized_pnl: snapshot.unrealized_pnl,
                    observed_at: snapshot.observed_at,
                }),
        };

        self.storage.save_account_monitor_state_row(&row).await
    }
}
