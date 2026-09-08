use std::collections::HashMap;
use std::sync::Arc;

use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountMonitor, ApplicationNotification, EffectJournalDiagnostics, SessionEffectQueue,
    TrackDebugQueryService, TrackRuntimeLifecycleService, TradingQueries, TradingRuntime,
};
use tokio::sync::broadcast;

use crate::account_projector::AccountProjector;
use crate::exchange_freshness::ExchangeFreshness;
use crate::pnl_audit::RecentFillsAuditor;
use crate::projector::TrackProjector;
use crate::runtime::{
    AccountMarginGuardStore, PnlBackfillStatus, RecoveryDirtyState, RuntimeHealth,
    TrackReconcileGuards,
};

#[derive(Clone)]
pub struct HttpState {
    pub runtime: Arc<TradingRuntime>,
    pub query_service: Arc<TradingQueries>,
    pub debug_query_service: Arc<TrackDebugQueryService>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
    pub runtime_health: Arc<RuntimeHealth>,
    pub pnl_backfill_status: Arc<PnlBackfillStatus>,
    pub effect_journal_diagnostics: Arc<EffectJournalDiagnostics>,
    pub exchange_freshness: Arc<ExchangeFreshness>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub recent_fills_auditor: Arc<RecentFillsAuditor>,
    pub configuration_metadata: Arc<HashMap<String, TrackConfigurationMetadata>>,
}

#[derive(Clone, Copy)]
pub struct TrackConfigurationMetadata {
    pub leverage: u32,
    pub tick_timeout_secs: u64,
}

#[derive(Clone)]
pub struct WebSocketState {
    pub notifications: broadcast::Sender<ApplicationNotification>,
    pub live_view_notifications: broadcast::Sender<String>,
    pub runtime: Arc<TradingRuntime>,
    pub query_service: Arc<TradingQueries>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
    #[cfg(test)]
    pub diagnostics_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::websocket::WebSocketDiagnosticsSnapshot>>,
}

#[derive(Clone)]
pub struct ReconcileState {
    pub runtime: Arc<TradingRuntime>,
    pub runtime_lifecycle_service: Arc<TrackRuntimeLifecycleService>,
    pub exchange_freshness: Arc<ExchangeFreshness>,
    pub reconcile_guards: Arc<TrackReconcileGuards>,
    pub recovery_dirty_state: Arc<RecoveryDirtyState>,
}

#[derive(Clone)]
pub struct RuntimeState {
    pub reconcile: ReconcileState,
    pub live_view_notifications: broadcast::Sender<String>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub runtime_health: Arc<RuntimeHealth>,
    pub pnl_backfill_status: Arc<PnlBackfillStatus>,
    pub recent_fills_auditor: Arc<RecentFillsAuditor>,
}

#[derive(Clone)]
pub struct EffectWorkerState {
    pub reconcile: ReconcileState,
    pub submit_effect_service: Arc<SubmitEffectService>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub session_effect_queue: SessionEffectQueue,
    pub runtime_health: Arc<RuntimeHealth>,
}
