use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use anyhow::Result;

use crate::server_context::RuntimeState;

use super::{RuntimeHealth, RuntimeHealthComponent, ServerRuntime};

pub(super) async fn refresh_once(state: &RuntimeState) -> Result<()> {
    refresh_monitor(&state.account_monitor, &state.runtime_health).await
}

pub(super) fn spawn_account_task(
    runtime: &ServerRuntime,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let state = runtime.state.clone();
    let refresh_interval = runtime.account_refresh_interval;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(refresh_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = refresh_once(&state).await {
                        tracing::warn!("account monitor refresh failed: {error}");
                    }
                }
            }
        }
    })
}

async fn refresh_monitor(
    account_monitor: &poise_application::AccountMonitor,
    runtime_health: &RuntimeHealth,
) -> Result<()> {
    if let Err(error) = account_monitor.refresh_once().await {
        runtime_health.record_error_now(RuntimeHealthComponent::AccountMonitor, error.to_string());
        Err(error)
    } else {
        runtime_health.record_success_now(RuntimeHealthComponent::AccountMonitor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use poise_application::{
        AccountMonitor, AccountMonitorConfig, AccountMonitorStore, ApplicationNotification,
        StoredAccountMonitorState,
    };
    use poise_engine::ports::{AccountSummaryPort, AccountSummarySnapshot};
    use tokio::sync::broadcast;

    use crate::runtime::{RuntimeHealth, RuntimeHealthComponent, RuntimeHealthStatus};

    struct SummarySource {
        snapshot: AccountSummarySnapshot,
    }

    #[async_trait::async_trait]
    impl AccountSummaryPort for SummarySource {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            Ok(self.snapshot.clone())
        }
    }

    struct FailingSummarySource;

    #[async_trait::async_trait]
    impl AccountSummaryPort for FailingSummarySource {
        async fn get_account_summary(&self) -> Result<AccountSummarySnapshot> {
            Err(anyhow::anyhow!("account summary unavailable"))
        }
    }

    struct NoopAccountMonitorStore;

    #[async_trait::async_trait]
    impl AccountMonitorStore for NoopAccountMonitorStore {
        async fn load_state(&self) -> Result<Option<StoredAccountMonitorState>> {
            Ok(None)
        }

        async fn save_state(&self, _state: &StoredAccountMonitorState) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn refresh_monitor_populates_current_summary_and_marks_health_ok() {
        let monitor = account_monitor_with_source(Arc::new(SummarySource {
            snapshot: AccountSummarySnapshot {
                equity: 12_500.0,
                available: 9_000.0,
                available_by_asset: BTreeMap::from([("BTC".to_string(), 0.25)]),
                unrealized_pnl: -350.0,
                observed_at: Utc.with_ymd_and_hms(2026, 4, 4, 1, 23, 45).unwrap(),
            },
        }))
        .await;
        let health = RuntimeHealth::new();

        super::refresh_monitor(&monitor, &health)
            .await
            .expect("account refresh should succeed");

        let summary = monitor
            .current_summary()
            .await
            .expect("explicit refresh should populate account summary");
        assert_eq!(summary.equity, 12_500.0);
        assert_eq!(
            summary.available_by_asset,
            BTreeMap::from([("BTC".to_string(), 0.25)])
        );
        assert_eq!(
            account_monitor_health_status(&health),
            RuntimeHealthStatus::Ok
        );
    }

    #[tokio::test]
    async fn refresh_monitor_returns_error_and_records_health_error_without_current_summary() {
        let monitor = account_monitor_with_source(Arc::new(FailingSummarySource)).await;
        let health = RuntimeHealth::new();

        let error = super::refresh_monitor(&monitor, &health)
            .await
            .expect_err("account refresh should fail");

        assert_eq!(error.to_string(), "account summary unavailable");
        assert_eq!(monitor.current_summary().await, None);
        let task = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::AccountMonitor)
            .expect("account monitor health should exist");
        assert_eq!(task.status, RuntimeHealthStatus::Degraded);
        assert_eq!(
            task.last_error.as_deref(),
            Some("account summary unavailable")
        );
    }

    async fn account_monitor_with_source(source: Arc<dyn AccountSummaryPort>) -> AccountMonitor {
        let (notifications, _) = broadcast::channel::<ApplicationNotification>(1);
        AccountMonitor::restore(
            source,
            Arc::new(NoopAccountMonitorStore),
            notifications,
            AccountMonitorConfig::default(),
        )
        .await
        .expect("account monitor should restore")
    }

    fn account_monitor_health_status(health: &RuntimeHealth) -> RuntimeHealthStatus {
        health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::AccountMonitor)
            .expect("account monitor health should exist")
            .status
    }
}
