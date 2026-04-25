use std::sync::Arc;

use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountMonitor, ApplicationNotification, SessionEffectQueue, TrackCommandService,
    TrackDebugQueryService, TrackEffectService, TrackEffectStore, TrackObservationService,
    TrackQueryService, TrackRuntimeLifecycleService,
};
use tokio::sync::broadcast;

use crate::account_projector::AccountProjector;
use crate::exchange_freshness::ExchangeFreshness;
use crate::projector::TrackProjector;
use crate::runtime::{AccountMarginGuardStore, RecoveryDirtyState, TrackReconcileGuards};
use crate::submit_preflight::SubmitPreflight;

#[derive(Clone)]
pub struct HttpState {
    pub command_service: Arc<TrackCommandService>,
    pub query_service: Arc<TrackQueryService>,
    pub debug_query_service: Arc<TrackDebugQueryService>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
}

#[derive(Clone)]
pub struct WebSocketState {
    pub notifications: broadcast::Sender<ApplicationNotification>,
    pub live_view_notifications: broadcast::Sender<String>,
    pub observation_service: Arc<TrackObservationService>,
    pub query_service: Arc<TrackQueryService>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
    #[cfg(test)]
    pub diagnostics_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::websocket::WebSocketDiagnosticsSnapshot>>,
}

#[derive(Clone)]
pub struct ReconcileState {
    pub observation_service: Arc<TrackObservationService>,
    pub runtime_lifecycle_service: Arc<TrackRuntimeLifecycleService>,
    pub effect_store: Arc<dyn TrackEffectStore>,
    pub exchange_freshness: Arc<ExchangeFreshness>,
    pub reconcile_guards: Arc<TrackReconcileGuards>,
    pub submit_preflight: Arc<SubmitPreflight>,
    pub recovery_dirty_state: Arc<RecoveryDirtyState>,
}

#[derive(Clone)]
pub struct RuntimeState {
    pub reconcile: ReconcileState,
    pub notifications: broadcast::Sender<ApplicationNotification>,
    pub live_view_notifications: broadcast::Sender<String>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
}

#[derive(Clone)]
pub struct EffectWorkerState {
    pub reconcile: ReconcileState,
    pub effect_service: Arc<TrackEffectService>,
    pub submit_effect_service: Arc<SubmitEffectService>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub session_effect_queue: SessionEffectQueue,
}
