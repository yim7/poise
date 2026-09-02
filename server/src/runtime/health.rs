use std::collections::BTreeMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use poise_application::{
    AccountReadModel, AccountRiskSignal, EffectJournalDiagnosticsSnapshot, TrackListReadModel,
    TrackPriceExecutionBlockReason, TrackReadStatus, TrackRecoveryIssue,
};
use poise_protocol::{ReadinessIssueView, TradingReadinessResponse};

use crate::exchange_freshness::StaleExchangeStateSnapshot;

use super::guards::AccountMarginBlock;
use super::pnl_backfill::PnlBackfillSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RuntimeHealthComponent {
    MarketData,
    UserData,
    EffectWorker,
    Recovery,
    AccountMonitor,
}

impl RuntimeHealthComponent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MarketData => "market_data",
            Self::UserData => "user_data",
            Self::EffectWorker => "effect_worker",
            Self::Recovery => "recovery",
            Self::AccountMonitor => "account_monitor",
        }
    }

    fn all() -> [Self; 5] {
        [
            Self::MarketData,
            Self::UserData,
            Self::EffectWorker,
            Self::Recovery,
            Self::AccountMonitor,
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
    scoped_tasks: BTreeMap<RuntimeHealthComponent, BTreeMap<RuntimeHealthScope, RuntimeTaskHealth>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeTaskHealth {
    last_success_at: Option<DateTime<Utc>>,
    last_error_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

impl RuntimeTaskHealth {
    fn new() -> Self {
        Self {
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
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RuntimeHealthScope {
    Global,
    Track(String),
    TrackHealth(String),
}

#[derive(Debug)]
pub(crate) struct RuntimeHealth {
    tasks:
        RwLock<BTreeMap<RuntimeHealthComponent, BTreeMap<RuntimeHealthScope, RuntimeTaskHealth>>>,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        let tasks = RuntimeHealthComponent::all()
            .into_iter()
            .map(|component| {
                (
                    component,
                    BTreeMap::from([(RuntimeHealthScope::Global, RuntimeTaskHealth::new())]),
                )
            })
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
        let probe = tasks
            .entry(component)
            .or_default()
            .entry(RuntimeHealthScope::Global)
            .or_insert_with(RuntimeTaskHealth::new);
        probe.last_success_at = Some(at);
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
        let probe = tasks
            .entry(component)
            .or_default()
            .entry(RuntimeHealthScope::Global)
            .or_insert_with(RuntimeTaskHealth::new);
        probe.last_error_at = Some(at);
        probe.last_error = Some(error.to_string());
    }

    pub(crate) fn record_error_now(&self, component: RuntimeHealthComponent, error: impl ToString) {
        self.record_error(component, Utc::now(), error);
    }

    pub(crate) fn register_track_scopes(
        &self,
        component: RuntimeHealthComponent,
        track_ids: Vec<String>,
    ) {
        let mut tasks = self.tasks.write().unwrap();
        let probes = tasks.entry(component).or_default();
        if probes
            .get(&RuntimeHealthScope::Global)
            .is_some_and(|probe| probe.status() == RuntimeHealthStatus::Unknown)
        {
            probes.remove(&RuntimeHealthScope::Global);
        }
        for track_id in track_ids {
            probes
                .entry(RuntimeHealthScope::Track(track_id))
                .or_insert_with(RuntimeTaskHealth::new);
        }
    }

    fn record_scope_success(
        &self,
        component: RuntimeHealthComponent,
        scope: RuntimeHealthScope,
        at: DateTime<Utc>,
    ) {
        let mut tasks = self.tasks.write().unwrap();
        let probe = tasks
            .entry(component)
            .or_default()
            .entry(scope)
            .or_insert_with(RuntimeTaskHealth::new);
        probe.last_success_at = Some(at);
    }

    pub(crate) fn record_track_success(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
        at: DateTime<Utc>,
    ) {
        self.record_scope_success(
            component,
            RuntimeHealthScope::Track(track_id.to_string()),
            at,
        );
    }

    pub(crate) fn record_track_success_now(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
    ) {
        self.record_track_success(component, track_id, Utc::now());
    }

    pub(crate) fn record_track_health_success_now(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
    ) {
        self.record_scope_success(
            component,
            RuntimeHealthScope::TrackHealth(track_id.to_string()),
            Utc::now(),
        );
    }

    fn record_scope_error(
        &self,
        component: RuntimeHealthComponent,
        scope: RuntimeHealthScope,
        at: DateTime<Utc>,
        error: impl ToString,
    ) {
        let mut tasks = self.tasks.write().unwrap();
        let probe = tasks
            .entry(component)
            .or_default()
            .entry(scope)
            .or_insert_with(RuntimeTaskHealth::new);
        probe.last_error_at = Some(at);
        probe.last_error = Some(error.to_string());
    }

    pub(crate) fn record_track_error(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
        at: DateTime<Utc>,
        error: impl ToString,
    ) {
        self.record_scope_error(
            component,
            RuntimeHealthScope::Track(track_id.to_string()),
            at,
            error,
        );
    }

    pub(crate) fn record_track_error_now(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
        error: impl ToString,
    ) {
        self.record_track_error(component, track_id, Utc::now(), error);
    }

    pub(crate) fn record_track_health_error(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
        at: DateTime<Utc>,
        error: impl ToString,
    ) {
        self.record_scope_error(
            component,
            RuntimeHealthScope::TrackHealth(track_id.to_string()),
            at,
            error,
        );
    }

    pub(crate) fn record_track_health_error_now(
        &self,
        component: RuntimeHealthComponent,
        track_id: &str,
        error: impl ToString,
    ) {
        self.record_track_health_error(component, track_id, Utc::now(), error);
    }

    pub(crate) fn snapshot(&self) -> RuntimeHealthSnapshot {
        let scoped_tasks = self.tasks.read().unwrap().clone();
        let tasks = scoped_tasks
            .iter()
            .map(|(component, probes)| aggregate_task_health(*component, probes))
            .collect();
        RuntimeHealthSnapshot {
            tasks,
            scoped_tasks,
        }
    }
}

fn aggregate_task_health(
    component: RuntimeHealthComponent,
    probes: &BTreeMap<RuntimeHealthScope, RuntimeTaskHealth>,
) -> RuntimeTaskHealthSnapshot {
    let status = if probes
        .values()
        .any(|probe| probe.status() == RuntimeHealthStatus::Degraded)
    {
        RuntimeHealthStatus::Degraded
    } else if probes.is_empty()
        || probes
            .values()
            .any(|probe| probe.status() == RuntimeHealthStatus::Unknown)
    {
        RuntimeHealthStatus::Unknown
    } else {
        RuntimeHealthStatus::Ok
    };
    let last_success_at = probes
        .values()
        .filter_map(|probe| probe.last_success_at)
        .max();
    let current_error = probes
        .values()
        .filter(|probe| probe.status() == RuntimeHealthStatus::Degraded)
        .filter_map(|probe| {
            probe
                .last_error_at
                .map(|last_error_at| (last_error_at, probe.last_error.clone()))
        })
        .max_by_key(|(last_error_at, _)| *last_error_at);
    let historical_error = probes
        .values()
        .filter_map(|probe| {
            probe
                .last_error_at
                .map(|last_error_at| (last_error_at, probe.last_error.clone()))
        })
        .max_by_key(|(last_error_at, _)| *last_error_at);
    let (last_error_at, last_error) = current_error
        .or(historical_error)
        .map(|(at, error)| (Some(at), error))
        .unwrap_or((None, None));

    RuntimeTaskHealthSnapshot {
        component,
        status,
        last_success_at,
        last_error_at,
        last_error,
    }
}

pub(crate) fn assess_trading_readiness(
    tracks: &[TrackListReadModel],
    account: Option<&AccountReadModel>,
    runtime_health: &RuntimeHealthSnapshot,
    pnl_backfill: &PnlBackfillSnapshot,
    effect_journal: &EffectJournalDiagnosticsSnapshot,
    account_margin_block: &AccountMarginBlock,
    stale_exchange_state: &[StaleExchangeStateSnapshot],
) -> TradingReadinessResponse {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    for task in readiness_runtime_tasks(runtime_health, tracks) {
        let component = task.component.as_str().to_string();
        match task.status {
            RuntimeHealthStatus::Unknown => blockers.push(ReadinessIssueView {
                component,
                reason: format!(
                    "runtime task `{}` has not reported a successful observation",
                    task.component.as_str()
                ),
            }),
            RuntimeHealthStatus::Degraded => blockers.push(ReadinessIssueView {
                component,
                reason: task.last_error.clone().unwrap_or_else(|| {
                    format!("runtime task `{}` is degraded", task.component.as_str())
                }),
            }),
            RuntimeHealthStatus::Ok => {}
        }
    }

    let pnl_status = pnl_backfill.health_status();
    if pnl_backfill.unsupported_tracks > 0 {
        blockers.push(ReadinessIssueView {
            component: "pnl_backfill".to_string(),
            reason: format!(
                "required recent PNL recovery is unsupported for {} configured track(s)",
                pnl_backfill.unsupported_tracks
            ),
        });
    } else if pnl_backfill.incomplete_tracks > 0 {
        blockers.push(ReadinessIssueView {
            component: "pnl_backfill".to_string(),
            reason: format!(
                "required recent PNL window is incomplete for {} configured track(s)",
                pnl_backfill.incomplete_tracks
            ),
        });
    } else if pnl_status == RuntimeHealthStatus::Unknown {
        blockers.push(ReadinessIssueView {
            component: "pnl_backfill".to_string(),
            reason: "PNL capability and backfill have not completed successfully".to_string(),
        });
    } else if pnl_status == RuntimeHealthStatus::Degraded {
        blockers.push(ReadinessIssueView {
            component: "pnl_backfill".to_string(),
            reason: pnl_backfill
                .last_error
                .clone()
                .unwrap_or_else(|| "PNL backfill is degraded".to_string()),
        });
    }

    match account {
        None => blockers.push(ReadinessIssueView {
            component: "account_monitor".to_string(),
            reason: "no current account summary is available".to_string(),
        }),
        Some(account) if account.risk_signal == AccountRiskSignal::Critical => {
            blockers.push(ReadinessIssueView {
                component: "account_monitor".to_string(),
                reason: account
                    .reason
                    .clone()
                    .unwrap_or_else(|| "account risk signal is critical".to_string()),
            });
        }
        Some(account) if account.risk_signal == AccountRiskSignal::Attention => {
            warnings.push(ReadinessIssueView {
                component: "account_monitor".to_string(),
                reason: account
                    .reason
                    .clone()
                    .unwrap_or_else(|| "account risk signal requires attention".to_string()),
            });
        }
        Some(_) => {}
    }

    if account_margin_block.increase_blocked {
        blockers.push(ReadinessIssueView {
            component: "account_capacity".to_string(),
            reason: account_margin_block
                .blocked_reason
                .clone()
                .unwrap_or_else(|| "account margin guard blocks inventory increases".to_string()),
        });
    }

    for track in tracks {
        let requires_live_execution_inputs = track_requires_live_execution_inputs(track);
        if requires_live_execution_inputs {
            if track.status == TrackReadStatus::WaitingMarketData {
                blockers.push(track_issue(
                    "market_data",
                    track,
                    "is waiting for its first usable market observation",
                ));
            }
            if track.has_stale_market_data {
                blockers.push(track_issue("market_data", track, "has stale market data"));
            }
            if let Some(reason) = track.price_execution_block_reason {
                blockers.push(track_issue(
                    "market_data",
                    track,
                    match reason {
                        TrackPriceExecutionBlockReason::MissingExecutionQuote => {
                            "has no usable execution quote"
                        }
                        TrackPriceExecutionBlockReason::MarkBookDivergence => {
                            "has unsafe mark/book divergence"
                        }
                    },
                ));
            }
        }
        if let Some(issue) = track.recovery_issue {
            blockers.push(track_issue(
                "recovery",
                track,
                match issue {
                    TrackRecoveryIssue::UnknownLiveOrder => "has an unknown live order",
                    TrackRecoveryIssue::DuplicateLiveOrders => "has duplicate live orders",
                    TrackRecoveryIssue::AmbiguousLiveOrder => "has an ambiguous live order",
                    TrackRecoveryIssue::ExpectedExposureMismatch => {
                        "has an unexplained exposure mismatch"
                    }
                    TrackRecoveryIssue::BoundaryProgressOutOfRange => {
                        "has invalid boundary progress"
                    }
                },
            ));
        }
        if track.has_account_margin_guard {
            blockers.push(track_issue(
                "account_capacity",
                track,
                "is blocked by the account margin guard",
            ));
        }
    }

    for stale in stale_exchange_state {
        blockers.push(ReadinessIssueView {
            component: "exchange_state".to_string(),
            reason: format!(
                "track `{}` requires reconciliation because {}",
                stale.track_id,
                stale.reason.readiness_reason()
            ),
        });
    }

    if effect_journal.failure_count > 0 {
        let reason = effect_journal
            .last_failure
            .as_ref()
            .map(|failure| {
                format!(
                    "{} effect journal write failure(s) in this session; last failure for track `{}` during `{}`: {}",
                    effect_journal.failure_count,
                    failure.track_id.as_str(),
                    failure.operation,
                    failure.error
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{} effect journal write failure(s) in this session",
                    effect_journal.failure_count
                )
            });
        warnings.push(ReadinessIssueView {
            component: "effect_journal".to_string(),
            reason,
        });
    }

    TradingReadinessResponse {
        ready_to_trade: blockers.is_empty(),
        blockers,
        warnings,
    }
}

fn readiness_runtime_tasks(
    runtime_health: &RuntimeHealthSnapshot,
    tracks: &[TrackListReadModel],
) -> Vec<RuntimeTaskHealthSnapshot> {
    runtime_health
        .tasks
        .iter()
        .filter_map(|task| {
            if task.component != RuntimeHealthComponent::MarketData {
                return Some(task.clone());
            }
            if !tracks.iter().any(track_requires_live_execution_inputs) {
                return None;
            }

            let relevant_scopes = runtime_health
                .scoped_tasks
                .get(&RuntimeHealthComponent::MarketData)
                .into_iter()
                .flat_map(|scopes| scopes.iter())
                .filter(|(scope, _)| market_data_scope_is_relevant(scope, tracks))
                .map(|(scope, probe)| (scope.clone(), probe.clone()))
                .collect();
            Some(aggregate_task_health(
                RuntimeHealthComponent::MarketData,
                &relevant_scopes,
            ))
        })
        .collect()
}

fn market_data_scope_is_relevant(
    scope: &RuntimeHealthScope,
    tracks: &[TrackListReadModel],
) -> bool {
    match scope {
        RuntimeHealthScope::Global => true,
        RuntimeHealthScope::Track(track_id) | RuntimeHealthScope::TrackHealth(track_id) => tracks
            .iter()
            .find(|track| track.track_id == *track_id)
            .map(track_requires_live_execution_inputs)
            .unwrap_or(true),
    }
}

fn track_issue(component: &str, track: &TrackListReadModel, reason: &str) -> ReadinessIssueView {
    ReadinessIssueView {
        component: component.to_string(),
        reason: format!("track `{}` {reason}", track.track_id),
    }
}

fn track_requires_live_execution_inputs(track: &TrackListReadModel) -> bool {
    match track.status {
        TrackReadStatus::Paused => false,
        TrackReadStatus::Terminated => {
            track.current_exposure.abs() > f64::EPSILON || track.active_binding_count > 0
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use poise_application::{
        AccountReadModel, AccountRiskSignal, EffectJournalDiagnosticsSnapshot,
        EffectJournalFailure, TrackInventoryDecisionReadModel, TrackListReadModel,
        TrackPriceExecutionBlockReason, TrackReadStatus, TrackRecoveryIssue,
        TrackStrategyPriceStatus,
    };
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_engine::execution_plan::DecisionReason;
    use poise_engine::ledger::TrackPnlStats;

    use crate::runtime::PnlBackfillSnapshot;
    use crate::test_support::test_instrument_spec;

    use super::{
        AccountMarginBlock, RuntimeHealth, RuntimeHealthComponent, RuntimeHealthStatus,
        assess_trading_readiness,
    };

    fn healthy_track() -> TrackListReadModel {
        let instrument_spec = test_instrument_spec();
        TrackListReadModel {
            track_id: "btc-core".to_string(),
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            instrument_spec: instrument_spec.clone(),
            status: TrackReadStatus::Active,
            updated_at: Utc.with_ymd_and_hms(2026, 6, 17, 3, 0, 0).unwrap(),
            strategy_price: Some(100.0),
            strategy_price_status: TrackStrategyPriceStatus::Live,
            current_exposure: 0.0,
            position_qty: 0.0,
            signed_position_notional: Some(
                poise_core::instrument::Amount::new(0.0, instrument_spec.notional_asset().clone())
                    .unwrap(),
            ),
            contract_base_exposure: None,
            decision: TrackInventoryDecisionReadModel {
                revision: Some(1),
                policy_target: Some(0.0),
                risk_approved_target: Some(0.0),
                acquisition_frontier: None,
                execution_target: Some(0.0),
                reason: DecisionReason::AtExecutionTarget,
            },
            acquisition_session: None,
            risk_acquisition: None,
            pnl: TrackPnlStats::default().view(instrument_spec.settlement_asset(), 0.0),
            recovery_issue: None,
            has_account_margin_guard: false,
            has_stale_market_data: false,
            price_execution_block_reason: None,
            active_binding_count: 0,
        }
    }

    fn healthy_account() -> AccountReadModel {
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 0, 0).unwrap();
        AccountReadModel {
            valuation_asset: poise_core::instrument::Asset::try_new("USD").unwrap(),
            equity: 10_000.0,
            available: Some(8_000.0),
            available_by_asset: Default::default(),
            unrealized_pnl: Some(0.0),
            baseline_equity: 10_000.0,
            day_base_at: at,
            day_change_pct: Some(0.0),
            risk_signal: AccountRiskSignal::Normal,
            reason: None,
            updated_at: at,
        }
    }

    fn journal_warning() -> EffectJournalDiagnosticsSnapshot {
        EffectJournalDiagnosticsSnapshot {
            failure_count: 1,
            last_failure: Some(EffectJournalFailure {
                observed_at: Utc.with_ymd_and_hms(2026, 6, 17, 3, 1, 0).unwrap(),
                track_id: TrackId::new("btc-core"),
                operation: "append_entries".to_string(),
                error: "database read-only".to_string(),
            }),
        }
    }

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

        health.record_success(RuntimeHealthComponent::Recovery, first);
        health.record_error(RuntimeHealthComponent::Recovery, second, "temporary outage");
        let degraded = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::Recovery)
            .unwrap();
        assert_eq!(degraded.status, RuntimeHealthStatus::Degraded);
        assert_eq!(degraded.last_success_at, Some(first));
        assert_eq!(degraded.last_error_at, Some(second));
        assert_eq!(degraded.last_error.as_deref(), Some("temporary outage"));

        health.record_success(RuntimeHealthComponent::Recovery, third);
        let recovered = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::Recovery)
            .unwrap();
        assert_eq!(recovered.status, RuntimeHealthStatus::Ok);
        assert_eq!(recovered.last_success_at, Some(third));
        assert_eq!(recovered.last_error_at, Some(second));
        assert_eq!(recovered.last_error.as_deref(), Some("temporary outage"));
    }

    #[test]
    fn scoped_runtime_health_requires_every_track_and_does_not_cross_clear_errors() {
        let health = RuntimeHealth::new();
        let first = Utc.with_ymd_and_hms(2026, 6, 17, 2, 0, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 6, 17, 2, 1, 0).unwrap();
        let third = Utc.with_ymd_and_hms(2026, 6, 17, 2, 2, 0).unwrap();

        health.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec!["btc-core".to_string(), "eth-core".to_string()],
        );
        health.record_track_success(RuntimeHealthComponent::MarketData, "btc-core", first);
        assert_eq!(
            health
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.component == RuntimeHealthComponent::MarketData)
                .unwrap()
                .status,
            RuntimeHealthStatus::Unknown,
            "one healthy track must not hide another track that has never become healthy"
        );

        health.record_track_error(
            RuntimeHealthComponent::MarketData,
            "eth-core",
            second,
            "ETH market receiver closed",
        );
        health.record_track_success(RuntimeHealthComponent::MarketData, "btc-core", third);
        assert_eq!(
            health
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.component == RuntimeHealthComponent::MarketData)
                .unwrap()
                .status,
            RuntimeHealthStatus::Degraded,
            "BTC success must not clear ETH failure"
        );

        health.record_track_success(RuntimeHealthComponent::MarketData, "eth-core", third);
        assert_eq!(
            health
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.component == RuntimeHealthComponent::MarketData)
                .unwrap()
                .status,
            RuntimeHealthStatus::Ok
        );
    }

    #[test]
    fn registering_track_scopes_preserves_an_earlier_health_checker_error() {
        let health = RuntimeHealth::new();
        let first = Utc.with_ymd_and_hms(2026, 6, 17, 2, 0, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 6, 17, 2, 1, 0).unwrap();

        health.record_track_health_error(
            RuntimeHealthComponent::MarketData,
            "btc-core",
            first,
            "failed to refresh stale-market state",
        );
        health.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec!["btc-core".to_string()],
        );
        health.record_track_success(RuntimeHealthComponent::MarketData, "btc-core", second);

        let market = health
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.component == RuntimeHealthComponent::MarketData)
            .unwrap();
        assert_eq!(market.status, RuntimeHealthStatus::Degraded);
        assert_eq!(
            market.last_error.as_deref(),
            Some("failed to refresh stale-market state")
        );
    }

    #[test]
    fn typed_health_scopes_do_not_collide_with_valid_track_ids() {
        let first = Utc.with_ymd_and_hms(2026, 6, 17, 2, 0, 0).unwrap();
        let second = Utc.with_ymd_and_hms(2026, 6, 17, 2, 1, 0).unwrap();

        let global_collision = RuntimeHealth::new();
        global_collision.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec!["__task__".to_string()],
        );
        global_collision.record_track_error(
            RuntimeHealthComponent::MarketData,
            "__task__",
            first,
            "track subscription failed",
        );
        global_collision.record_success(RuntimeHealthComponent::MarketData, second);
        assert_eq!(
            global_collision
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.component == RuntimeHealthComponent::MarketData)
                .unwrap()
                .status,
            RuntimeHealthStatus::Degraded,
            "global success must not clear a track whose id matches the old string sentinel"
        );

        let suffix_collision = RuntimeHealth::new();
        suffix_collision.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec!["btc:health".to_string()],
        );
        suffix_collision.record_track_health_error(
            RuntimeHealthComponent::MarketData,
            "btc",
            first,
            "BTC stale checker failed",
        );
        suffix_collision.record_track_success(
            RuntimeHealthComponent::MarketData,
            "btc:health",
            second,
        );
        assert_eq!(
            suffix_collision
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.component == RuntimeHealthComponent::MarketData)
                .unwrap()
                .status,
            RuntimeHealthStatus::Degraded,
            "track success must not clear another track's health-checker error"
        );
    }

    #[test]
    fn trading_readiness_blocks_unknown_tasks_but_keeps_journal_gap_as_warning() {
        let health = RuntimeHealth::new();
        let assessment = assess_trading_readiness(
            &[healthy_track()],
            Some(&healthy_account()),
            &health.snapshot(),
            &PnlBackfillSnapshot::default(),
            &journal_warning(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(!assessment.ready_to_trade);
        for component in [
            "market_data",
            "user_data",
            "effect_worker",
            "recovery",
            "account_monitor",
            "pnl_backfill",
        ] {
            assert!(
                assessment
                    .blockers
                    .iter()
                    .any(|issue| issue.component == component),
                "missing Unknown blocker for {component}"
            );
        }
        assert_eq!(assessment.warnings.len(), 1);
        assert_eq!(assessment.warnings[0].component, "effect_journal");
    }

    #[test]
    fn trading_readiness_allows_only_a_sticky_journal_warning() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        for component in RuntimeHealthComponent::all() {
            health.record_success(component, at);
        }
        let assessment = assess_trading_readiness(
            &[healthy_track()],
            Some(&healthy_account()),
            &health.snapshot(),
            &PnlBackfillSnapshot {
                last_success_at: Some(at),
                ..PnlBackfillSnapshot::default()
            },
            &journal_warning(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(assessment.ready_to_trade);
        assert!(assessment.blockers.is_empty());
        assert_eq!(assessment.warnings.len(), 1);
    }

    #[test]
    fn trading_readiness_blocks_incomplete_recent_pnl_coverage() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        for component in RuntimeHealthComponent::all() {
            health.record_success(component, at);
        }

        let assessment = assess_trading_readiness(
            &[healthy_track()],
            Some(&healthy_account()),
            &health.snapshot(),
            &PnlBackfillSnapshot {
                last_completed_at: Some(at),
                available_tracks: 1,
                incomplete_tracks: 1,
                last_error_at: Some(at),
                last_error: Some("recent PNL coverage is incomplete".to_string()),
                ..PnlBackfillSnapshot::default()
            },
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(!assessment.ready_to_trade);
        assert!(assessment.blockers.iter().any(|issue| {
            issue.component == "pnl_backfill" && issue.reason.contains("incomplete")
        }));
    }

    #[test]
    fn trading_readiness_projects_current_track_and_account_safety_facts() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        for component in RuntimeHealthComponent::all() {
            health.record_success(component, at);
        }
        let mut track = healthy_track();
        track.status = TrackReadStatus::WaitingMarketData;
        track.has_stale_market_data = true;
        track.has_account_margin_guard = true;
        track.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MarkBookDivergence);
        track.recovery_issue = Some(TrackRecoveryIssue::UnknownLiveOrder);
        let mut account = healthy_account();
        account.risk_signal = AccountRiskSignal::Critical;
        account.reason = Some("equity <= 0".to_string());

        let assessment = assess_trading_readiness(
            &[track],
            Some(&account),
            &health.snapshot(),
            &PnlBackfillSnapshot {
                last_success_at: Some(at),
                ..PnlBackfillSnapshot::default()
            },
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(!assessment.ready_to_trade);
        for component in ["market_data", "recovery", "account_capacity"] {
            assert!(
                assessment
                    .blockers
                    .iter()
                    .any(|issue| issue.component == component),
                "missing current-fact blocker for {component}"
            );
        }
        assert!(assessment.blockers.iter().any(|issue| {
            issue.component == "account_monitor" && issue.reason.contains("equity <= 0")
        }));
    }

    #[test]
    fn paused_track_does_not_require_live_execution_inputs() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        let mut track = healthy_track();
        track.status = TrackReadStatus::Paused;
        track.has_stale_market_data = true;
        track.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote);
        for component in RuntimeHealthComponent::all() {
            if component != RuntimeHealthComponent::MarketData {
                health.record_success(component, at);
            }
        }
        health.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec![track.track_id.clone()],
        );
        health.record_track_error(
            RuntimeHealthComponent::MarketData,
            &track.track_id,
            at,
            "paused track market subscription failed",
        );

        let assessment = assess_trading_readiness(
            &[track],
            Some(&healthy_account()),
            &health.snapshot(),
            &PnlBackfillSnapshot {
                last_success_at: Some(at),
                ..PnlBackfillSnapshot::default()
            },
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(assessment.ready_to_trade);
        assert!(assessment.blockers.is_empty());
    }

    #[test]
    fn inactive_track_market_error_does_not_mask_an_active_healthy_scope() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        for component in RuntimeHealthComponent::all() {
            if component != RuntimeHealthComponent::MarketData {
                health.record_success(component, at);
            }
        }
        let active = healthy_track();
        let mut paused = healthy_track();
        paused.track_id = "eth-paused".to_string();
        paused.status = TrackReadStatus::Paused;
        health.register_track_scopes(
            RuntimeHealthComponent::MarketData,
            vec![active.track_id.clone(), paused.track_id.clone()],
        );
        health.record_track_success(RuntimeHealthComponent::MarketData, &active.track_id, at);
        health.record_track_error(
            RuntimeHealthComponent::MarketData,
            &paused.track_id,
            at,
            "paused track market subscription failed",
        );

        let assessment = assess_trading_readiness(
            &[active, paused],
            Some(&healthy_account()),
            &health.snapshot(),
            &PnlBackfillSnapshot {
                last_success_at: Some(at),
                ..PnlBackfillSnapshot::default()
            },
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );

        assert!(assessment.ready_to_trade);
        assert!(assessment.blockers.is_empty());
    }

    #[test]
    fn terminated_track_requires_live_inputs_until_flat_and_without_bindings() {
        let health = RuntimeHealth::new();
        let at = Utc.with_ymd_and_hms(2026, 6, 17, 3, 2, 0).unwrap();
        for component in RuntimeHealthComponent::all() {
            health.record_success(component, at);
        }
        let pnl = PnlBackfillSnapshot {
            last_success_at: Some(at),
            ..PnlBackfillSnapshot::default()
        };
        let mut track = healthy_track();
        track.status = TrackReadStatus::Terminated;
        track.current_exposure = 1.0;
        track.price_execution_block_reason =
            Some(TrackPriceExecutionBlockReason::MissingExecutionQuote);

        let reducing = assess_trading_readiness(
            &[track.clone()],
            Some(&healthy_account()),
            &health.snapshot(),
            &pnl,
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );
        assert!(!reducing.ready_to_trade);
        assert!(
            reducing
                .blockers
                .iter()
                .any(|issue| issue.component == "market_data")
        );

        track.current_exposure = 0.0;
        track.active_binding_count = 0;
        let flat = assess_trading_readiness(
            &[track],
            Some(&healthy_account()),
            &health.snapshot(),
            &pnl,
            &EffectJournalDiagnosticsSnapshot::default(),
            &AccountMarginBlock::default(),
            &[],
        );
        assert!(flat.ready_to_trade);
        assert!(flat.blockers.is_empty());
    }
}
