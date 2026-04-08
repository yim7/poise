use std::sync::Arc;

use anyhow::{Result, ensure};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use poise_engine::ports::{AccountSummaryPort, AccountSummarySnapshot};
use serde::Deserialize;
use tokio::sync::{RwLock, broadcast};

use crate::{
    AccountMonitorStore, AccountReadModel, AccountRiskSignal, ApplicationNotification,
    StoredAccountMonitorState,
};

pub type ObservedAccountSnapshot = AccountSummarySnapshot;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AccountMonitorConfig {
    #[serde(default = "default_day_change_attention_pct")]
    pub day_change_attention_pct: f64,
    #[serde(default = "default_day_change_critical_pct")]
    pub day_change_critical_pct: f64,
    #[serde(default = "default_available_ratio_attention_pct")]
    pub available_ratio_attention_pct: f64,
    #[serde(default = "default_available_ratio_critical_pct")]
    pub available_ratio_critical_pct: f64,
    #[serde(default = "default_unrealized_loss_attention_pct")]
    pub unrealized_loss_attention_pct: f64,
    #[serde(default = "default_unrealized_loss_critical_pct")]
    pub unrealized_loss_critical_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct InMemoryAccountMonitorState {
    pub trading_day: Option<NaiveDate>,
    pub baseline_equity: Option<f64>,
    pub baseline_captured_at: Option<DateTime<Utc>>,
    pub last_observed_account_snapshot: Option<ObservedAccountSnapshot>,
}

pub struct AccountMonitor {
    account_summary: Arc<dyn AccountSummaryPort>,
    store: Arc<dyn AccountMonitorStore>,
    notifications: broadcast::Sender<ApplicationNotification>,
    config: AccountMonitorConfig,
    state: RwLock<InMemoryAccountMonitorState>,
}

impl Default for AccountMonitorConfig {
    fn default() -> Self {
        Self {
            day_change_attention_pct: default_day_change_attention_pct(),
            day_change_critical_pct: default_day_change_critical_pct(),
            available_ratio_attention_pct: default_available_ratio_attention_pct(),
            available_ratio_critical_pct: default_available_ratio_critical_pct(),
            unrealized_loss_attention_pct: default_unrealized_loss_attention_pct(),
            unrealized_loss_critical_pct: default_unrealized_loss_critical_pct(),
        }
    }
}

impl AccountMonitorConfig {
    pub fn validate(&self) -> Result<()> {
        validate_threshold_pair(
            "day_change_attention_pct",
            self.day_change_attention_pct,
            "day_change_critical_pct",
            self.day_change_critical_pct,
        )?;
        validate_threshold_pair(
            "available_ratio_attention_pct",
            self.available_ratio_attention_pct,
            "available_ratio_critical_pct",
            self.available_ratio_critical_pct,
        )?;
        validate_threshold_pair(
            "unrealized_loss_attention_pct",
            self.unrealized_loss_attention_pct,
            "unrealized_loss_critical_pct",
            self.unrealized_loss_critical_pct,
        )?;
        Ok(())
    }
}

impl AccountMonitor {
    pub fn unavailable(
        notifications: broadcast::Sender<ApplicationNotification>,
        config: AccountMonitorConfig,
    ) -> Self {
        Self {
            account_summary: Arc::new(UnavailableAccountSummarySource),
            store: Arc::new(InMemoryAccountMonitorStore),
            notifications,
            config,
            state: RwLock::new(InMemoryAccountMonitorState::default()),
        }
    }

    pub async fn restore(
        account_summary: Arc<dyn AccountSummaryPort>,
        store: Arc<dyn AccountMonitorStore>,
        notifications: broadcast::Sender<ApplicationNotification>,
        config: AccountMonitorConfig,
    ) -> Result<Self> {
        let restored_state = store
            .load_state()
            .await?
            .map(in_memory_state_from_stored)
            .unwrap_or_default();

        Ok(Self {
            account_summary,
            store,
            notifications,
            config,
            state: RwLock::new(restored_state),
        })
    }

    pub async fn current_summary(&self) -> Option<AccountReadModel> {
        let state = self.state.read().await.clone();
        build_read_model(&state, &self.config, None)
    }

    pub async fn refresh_once(&self) -> Result<()> {
        let snapshot = self.account_summary.get_account_summary().await?;
        let (previous_state, next_state, before, after) = {
            let mut state = self.state.write().await;
            let previous_state = state.clone();
            let before = build_read_model(&previous_state, &self.config, None);
            apply_snapshot(&mut state, snapshot);
            let next_state = state.clone();
            let after = build_read_model(&next_state, &self.config, None);
            (previous_state, next_state, before, after)
        };

        let stored_state = stored_state_from_in_memory(&next_state)
            .expect("refreshed account monitor state should be persistable");
        if let Err(error) = self.store.save_state(&stored_state).await {
            let mut state = self.state.write().await;
            *state = previous_state;
            return Err(error);
        }

        if before != after && after.is_some() {
            let _ = self
                .notifications
                .send(ApplicationNotification::AccountChanged);
        }

        Ok(())
    }
}

fn in_memory_state_from_stored(state: StoredAccountMonitorState) -> InMemoryAccountMonitorState {
    InMemoryAccountMonitorState {
        trading_day: Some(state.trading_day),
        baseline_equity: Some(state.baseline_equity),
        baseline_captured_at: Some(state.baseline_captured_at),
        last_observed_account_snapshot: state.last_observed_account_snapshot,
    }
}

fn stored_state_from_in_memory(
    state: &InMemoryAccountMonitorState,
) -> Option<StoredAccountMonitorState> {
    Some(StoredAccountMonitorState {
        trading_day: state.trading_day?,
        baseline_equity: state.baseline_equity?,
        baseline_captured_at: state.baseline_captured_at?,
        last_observed_account_snapshot: state.last_observed_account_snapshot.clone(),
    })
}

fn apply_snapshot(state: &mut InMemoryAccountMonitorState, snapshot: ObservedAccountSnapshot) {
    let trading_day = trading_day_for(snapshot.observed_at);
    let should_reset_baseline = state.trading_day != Some(trading_day)
        || state.baseline_equity.is_none()
        || state.baseline_captured_at.is_none();

    if should_reset_baseline {
        state.trading_day = Some(trading_day);
        state.baseline_equity = Some(snapshot.equity);
        state.baseline_captured_at = Some(snapshot.observed_at);
    }

    state.last_observed_account_snapshot = Some(snapshot);
}

fn trading_day_for(observed_at: DateTime<Utc>) -> NaiveDate {
    observed_at
        .with_timezone(&FixedOffset::east_opt(8 * 60 * 60).expect("valid fixed offset"))
        .date_naive()
}

fn build_read_model(
    state: &InMemoryAccountMonitorState,
    config: &AccountMonitorConfig,
    snapshot: Option<ObservedAccountSnapshot>,
) -> Option<AccountReadModel> {
    let snapshot = snapshot.or_else(|| state.last_observed_account_snapshot.clone())?;
    let baseline_equity = state.baseline_equity.unwrap_or(snapshot.equity);
    let day_base_at = state.baseline_captured_at.unwrap_or(snapshot.observed_at);

    if snapshot.equity <= 0.0 {
        return Some(AccountReadModel {
            equity: snapshot.equity,
            available: snapshot.available,
            unrealized_pnl: snapshot.unrealized_pnl,
            baseline_equity,
            day_base_at,
            day_change_pct: None,
            risk_signal: AccountRiskSignal::Critical,
            reason: Some("equity <= 0".to_string()),
            updated_at: snapshot.observed_at,
        });
    }

    let day_change_pct = (baseline_equity > 0.0)
        .then_some(((snapshot.equity - baseline_equity) / baseline_equity) * 100.0);
    let available_ratio_pct = (snapshot.available / snapshot.equity) * 100.0;
    let unrealized_ratio_pct = (snapshot.unrealized_pnl / snapshot.equity) * 100.0;

    let mut risk_signal = AccountRiskSignal::Normal;
    let mut reasons = Vec::new();

    if let Some(value) = day_change_pct {
        let signal = classify_signal(
            value,
            config.day_change_attention_pct,
            config.day_change_critical_pct,
        );
        risk_signal = max_signal(risk_signal, signal);
        if signal != AccountRiskSignal::Normal {
            reasons.push(format!("day_change {value:.1}%"));
        }
    }

    let available_signal = classify_signal(
        available_ratio_pct,
        config.available_ratio_attention_pct,
        config.available_ratio_critical_pct,
    );
    risk_signal = max_signal(risk_signal, available_signal);
    if available_signal != AccountRiskSignal::Normal {
        reasons.push(format!("available {available_ratio_pct:.1}%"));
    }

    let unrealized_signal = classify_signal(
        unrealized_ratio_pct,
        config.unrealized_loss_attention_pct,
        config.unrealized_loss_critical_pct,
    );
    risk_signal = max_signal(risk_signal, unrealized_signal);
    if unrealized_signal != AccountRiskSignal::Normal {
        reasons.push(format!("unrealized_pnl {unrealized_ratio_pct:.1}%"));
    }

    Some(AccountReadModel {
        equity: snapshot.equity,
        available: snapshot.available,
        unrealized_pnl: snapshot.unrealized_pnl,
        baseline_equity,
        day_base_at,
        day_change_pct,
        risk_signal,
        reason: (!reasons.is_empty()).then_some(reasons.join(", ")),
        updated_at: snapshot.observed_at,
    })
}

fn classify_signal(value: f64, attention: f64, critical: f64) -> AccountRiskSignal {
    if value <= critical {
        AccountRiskSignal::Critical
    } else if value <= attention {
        AccountRiskSignal::Attention
    } else {
        AccountRiskSignal::Normal
    }
}

fn max_signal(left: AccountRiskSignal, right: AccountRiskSignal) -> AccountRiskSignal {
    if severity(right) > severity(left) {
        right
    } else {
        left
    }
}

fn severity(signal: AccountRiskSignal) -> u8 {
    match signal {
        AccountRiskSignal::Normal => 0,
        AccountRiskSignal::Attention => 1,
        AccountRiskSignal::Critical => 2,
    }
}

fn validate_threshold_pair(
    attention_name: &str,
    attention: f64,
    critical_name: &str,
    critical: f64,
) -> Result<()> {
    ensure!(attention.is_finite(), "{attention_name} must be finite");
    ensure!(critical.is_finite(), "{critical_name} must be finite");
    ensure!(
        attention >= critical,
        "{attention_name} must be greater than or equal to {critical_name}"
    );
    Ok(())
}

fn default_day_change_attention_pct() -> f64 {
    -3.0
}

fn default_day_change_critical_pct() -> f64 {
    -5.0
}

fn default_available_ratio_attention_pct() -> f64 {
    30.0
}

fn default_available_ratio_critical_pct() -> f64 {
    15.0
}

fn default_unrealized_loss_attention_pct() -> f64 {
    -5.0
}

fn default_unrealized_loss_critical_pct() -> f64 {
    -10.0
}

struct InMemoryAccountMonitorStore;

#[async_trait::async_trait]
impl AccountMonitorStore for InMemoryAccountMonitorStore {
    async fn load_state(&self) -> Result<Option<StoredAccountMonitorState>> {
        Ok(None)
    }

    async fn save_state(&self, _state: &StoredAccountMonitorState) -> Result<()> {
        Ok(())
    }
}

struct UnavailableAccountSummarySource;

#[async_trait::async_trait]
impl AccountSummaryPort for UnavailableAccountSummarySource {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
        Err(anyhow::anyhow!(
            "account monitor is unavailable in this server state"
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use tokio::sync::broadcast;

    use super::{
        AccountMonitor, AccountMonitorConfig, InMemoryAccountMonitorState,
        InMemoryAccountMonitorStore, ObservedAccountSnapshot, build_read_model,
    };
    use crate::{AccountRiskSignal, ApplicationNotification};
    use poise_engine::ports::AccountSummarySnapshot;

    #[test]
    fn marks_equity_below_zero_as_critical() {
        let summary = build_read_model(
            &InMemoryAccountMonitorState::default(),
            &AccountMonitorConfig::default(),
            Some(ObservedAccountSnapshot {
                equity: -10.0,
                available: 2.5,
                unrealized_pnl: -1.25,
                observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 2, 3).unwrap(),
            }),
        )
        .expect("summary should exist");

        assert_eq!(summary.risk_signal, AccountRiskSignal::Critical);
        assert_eq!(summary.day_change_pct, None);
        assert_eq!(summary.reason.as_deref(), Some("equity <= 0"));
    }

    #[test]
    fn build_read_model_exposes_required_fields_when_snapshot_exists() {
        let observed_at = Utc.with_ymd_and_hms(2026, 4, 4, 1, 2, 3).unwrap();
        let baseline_at = Utc.with_ymd_and_hms(2026, 4, 4, 0, 0, 1).unwrap();
        let summary = build_read_model(
            &InMemoryAccountMonitorState {
                trading_day: Some(observed_at.date_naive()),
                baseline_equity: Some(12_800.0),
                baseline_captured_at: Some(baseline_at),
                last_observed_account_snapshot: Some(ObservedAccountSnapshot {
                    equity: 12_500.0,
                    available: 9_000.0,
                    unrealized_pnl: -350.0,
                    observed_at,
                }),
            },
            &AccountMonitorConfig::default(),
            None,
        )
        .expect("summary should exist");

        assert_eq!(summary.equity, 12_500.0);
        assert_eq!(summary.available, 9_000.0);
        assert_eq!(summary.unrealized_pnl, -350.0);
        assert_eq!(summary.baseline_equity, 12_800.0);
        assert_eq!(summary.day_base_at, baseline_at);
        assert_eq!(summary.updated_at, observed_at);
    }

    struct SummaryOnlySource {
        snapshot: AccountSummarySnapshot,
    }

    #[async_trait::async_trait]
    impl poise_engine::ports::AccountSummaryPort for SummaryOnlySource {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    #[tokio::test]
    async fn account_monitor_can_be_built_from_summary_only_source() {
        let source: Arc<dyn poise_engine::ports::AccountSummaryPort> =
            Arc::new(SummaryOnlySource {
                snapshot: AccountSummarySnapshot {
                    equity: 12_500.0,
                    available: 9_000.0,
                    unrealized_pnl: -350.0,
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 2, 3).unwrap(),
                },
            });
        let (notifications, _) = broadcast::channel(1);
        let monitor = AccountMonitor::restore(
            source,
            Arc::new(InMemoryAccountMonitorStore),
            notifications,
            AccountMonitorConfig::default(),
        )
        .await
        .expect("summary-only source should be sufficient");

        monitor
            .refresh_once()
            .await
            .expect("summary-only source should refresh");
        let summary = monitor
            .current_summary()
            .await
            .expect("summary should exist");
        assert_eq!(summary.equity, 12_500.0);
        assert_eq!(summary.available, 9_000.0);
        assert_eq!(summary.unrealized_pnl, -350.0);
    }

    #[tokio::test]
    async fn refresh_once_emits_account_changed_notification() {
        let source: Arc<dyn poise_engine::ports::AccountSummaryPort> =
            Arc::new(SummaryOnlySource {
                snapshot: AccountSummarySnapshot {
                    equity: 12_500.0,
                    available: 9_000.0,
                    unrealized_pnl: -350.0,
                    observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 2, 3).unwrap(),
                },
            });
        let (notifications, _) = broadcast::channel(1);
        let monitor = AccountMonitor::restore(
            source,
            Arc::new(InMemoryAccountMonitorStore),
            notifications.clone(),
            AccountMonitorConfig::default(),
        )
        .await
        .unwrap();
        let mut receiver = notifications.subscribe();

        monitor.refresh_once().await.unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::AccountChanged
        );
    }
}
