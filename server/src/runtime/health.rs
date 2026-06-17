use std::collections::BTreeMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RuntimeHealthComponent {
    MarketData,
    UserData,
    EffectWorker,
    Recovery,
    AccountMonitor,
    PnlBackfill,
}

impl RuntimeHealthComponent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MarketData => "market_data",
            Self::UserData => "user_data",
            Self::EffectWorker => "effect_worker",
            Self::Recovery => "recovery",
            Self::AccountMonitor => "account_monitor",
            Self::PnlBackfill => "pnl_backfill",
        }
    }

    fn all() -> [Self; 6] {
        [
            Self::MarketData,
            Self::UserData,
            Self::EffectWorker,
            Self::Recovery,
            Self::AccountMonitor,
            Self::PnlBackfill,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeHealthStatus {
    Unknown,
    Ok,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeTaskHealthSnapshot {
    pub component: RuntimeHealthComponent,
    pub status: RuntimeHealthStatus,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeHealthSnapshot {
    pub tasks: Vec<RuntimeTaskHealthSnapshot>,
}

#[derive(Debug, Clone)]
struct RuntimeTaskHealth {
    component: RuntimeHealthComponent,
    last_success_at: Option<DateTime<Utc>>,
    last_error_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

impl RuntimeTaskHealth {
    fn new(component: RuntimeHealthComponent) -> Self {
        Self {
            component,
            last_success_at: None,
            last_error_at: None,
            last_error: None,
        }
    }

    fn status(&self) -> RuntimeHealthStatus {
        match (self.last_success_at, self.last_error_at) {
            (None, None) => RuntimeHealthStatus::Unknown,
            (Some(success), Some(error)) if error > success => RuntimeHealthStatus::Degraded,
            (None, Some(_)) => RuntimeHealthStatus::Degraded,
            _ => RuntimeHealthStatus::Ok,
        }
    }

    fn snapshot(&self) -> RuntimeTaskHealthSnapshot {
        RuntimeTaskHealthSnapshot {
            component: self.component,
            status: self.status(),
            last_success_at: self.last_success_at,
            last_error_at: self.last_error_at,
            last_error: self.last_error.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RuntimeHealth {
    tasks: RwLock<BTreeMap<RuntimeHealthComponent, RuntimeTaskHealth>>,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        let tasks = RuntimeHealthComponent::all()
            .into_iter()
            .map(|component| (component, RuntimeTaskHealth::new(component)))
            .collect();
        Self {
            tasks: RwLock::new(tasks),
        }
    }
}

impl RuntimeHealth {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_success(&self, component: RuntimeHealthComponent, at: DateTime<Utc>) {
        let mut tasks = self.tasks.write().unwrap();
        let task = tasks
            .entry(component)
            .or_insert_with(|| RuntimeTaskHealth::new(component));
        task.last_success_at = Some(at);
    }

    pub(crate) fn record_success_now(&self, component: RuntimeHealthComponent) {
        self.record_success(component, Utc::now());
    }

    pub(crate) fn record_error(
        &self,
        component: RuntimeHealthComponent,
        at: DateTime<Utc>,
        error: impl ToString,
    ) {
        let mut tasks = self.tasks.write().unwrap();
        let task = tasks
            .entry(component)
            .or_insert_with(|| RuntimeTaskHealth::new(component));
        task.last_error_at = Some(at);
        task.last_error = Some(error.to_string());
    }

    pub(crate) fn record_error_now(
        &self,
        component: RuntimeHealthComponent,
        error: impl ToString,
    ) {
        self.record_error(component, Utc::now(), error);
    }

    pub(crate) fn snapshot(&self) -> RuntimeHealthSnapshot {
        let tasks = self
            .tasks
            .read()
            .unwrap()
            .values()
            .map(RuntimeTaskHealth::snapshot)
            .collect();
        RuntimeHealthSnapshot { tasks }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{RuntimeHealth, RuntimeHealthComponent, RuntimeHealthStatus};

    #[test]
    fn runtime_health_starts_with_all_core_tasks_unknown() {
        let health = RuntimeHealth::new();

        let snapshot = health.snapshot();

        let components = snapshot
            .tasks
            .iter()
            .map(|task| task.component.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            components,
            vec![
                "market_data",
                "user_data",
                "effect_worker",
                "recovery",
                "account_monitor",
                "pnl_backfill",
            ]
        );
        assert!(
            snapshot
                .tasks
                .iter()
                .all(|task| task.status == RuntimeHealthStatus::Unknown)
        );
    }

    #[test]
    fn runtime_health_status_uses_latest_success_or_error() {
        let health = RuntimeHealth::new();
        let first = Utc.with_ymd_and_hms(2026, 6, 17, 1, 0, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 6, 17, 1, 1, 0).unwrap();
        let third = Utc.with_ymd_and_hms(2026, 6, 17, 1, 2, 0).unwrap();

        health.record_success(RuntimeHealthComponent::PnlBackfill, first);
        health.record_error(
            RuntimeHealthComponent::PnlBackfill,
            second,
            "temporary outage",
        );
        let degraded = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::PnlBackfill)
            .unwrap();
        assert_eq!(degraded.status, RuntimeHealthStatus::Degraded);
        assert_eq!(degraded.last_success_at, Some(first));
        assert_eq!(degraded.last_error_at, Some(second));
        assert_eq!(degraded.last_error.as_deref(), Some("temporary outage"));

        health.record_success(RuntimeHealthComponent::PnlBackfill, third);
        let recovered = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::PnlBackfill)
            .unwrap();
        assert_eq!(recovered.status, RuntimeHealthStatus::Ok);
        assert_eq!(recovered.last_success_at, Some(third));
        assert_eq!(recovered.last_error_at, Some(second));
        assert_eq!(recovered.last_error.as_deref(), Some("temporary outage"));
    }

}
