use std::sync::Arc;

use poise_application::{
    AccountMonitor, ApplicationNotification, TrackCommandService, TrackDebugQueryService,
    TrackEffectService, TrackEffectStore, TrackMutationStore, TrackObservationService,
    TrackQueryService,
};
use tokio::sync::broadcast;

use crate::account_projector::AccountProjector;
use crate::exchange_freshness::ExchangeFreshness;
use crate::projector::TrackProjector;
use crate::runtime::{AccountMarginGuardStore, TrackReconcileGuards};
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
    pub query_service: Arc<TrackQueryService>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
}

#[derive(Clone)]
pub struct ReconcileState {
    pub observation_service: Arc<TrackObservationService>,
    pub mutation_store: Arc<dyn TrackMutationStore>,
    pub effect_store: Arc<dyn TrackEffectStore>,
    pub exchange_freshness: Arc<ExchangeFreshness>,
    pub reconcile_guards: Arc<TrackReconcileGuards>,
    pub submit_preflight: Arc<SubmitPreflight>,
}

#[derive(Clone)]
pub struct RuntimeState {
    pub reconcile: ReconcileState,
    pub notifications: broadcast::Sender<ApplicationNotification>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
}

#[derive(Clone)]
pub struct EffectWorkerState {
    pub reconcile: ReconcileState,
    pub effect_service: Arc<TrackEffectService>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
}

#[cfg(test)]
#[derive(Clone)]
pub struct ServerState {
    pub command_service: Arc<TrackCommandService>,
    pub observation_service: Arc<TrackObservationService>,
    pub effect_service: Arc<TrackEffectService>,
    pub notifications: broadcast::Sender<ApplicationNotification>,
    pub mutation_store: Arc<dyn TrackMutationStore>,
    pub effect_store: Arc<dyn TrackEffectStore>,
    pub exchange_freshness: Arc<ExchangeFreshness>,
    pub query_service: Arc<TrackQueryService>,
    pub debug_query_service: Arc<TrackDebugQueryService>,
    pub projector: Arc<TrackProjector>,
    pub account_monitor: Arc<AccountMonitor>,
    pub account_projector: Arc<AccountProjector>,
    pub account_margin_guard: Arc<AccountMarginGuardStore>,
    pub reconcile_guards: Arc<TrackReconcileGuards>,
    pub submit_preflight: Arc<SubmitPreflight>,
}

#[cfg(test)]
impl ServerState {
    pub fn http_state(&self) -> HttpState {
        HttpState {
            command_service: Arc::clone(&self.command_service),
            query_service: Arc::clone(&self.query_service),
            debug_query_service: Arc::clone(&self.debug_query_service),
            projector: Arc::clone(&self.projector),
            account_monitor: Arc::clone(&self.account_monitor),
            account_projector: Arc::clone(&self.account_projector),
        }
    }

    pub fn websocket_state(&self) -> WebSocketState {
        WebSocketState {
            notifications: self.notifications.clone(),
            query_service: Arc::clone(&self.query_service),
            projector: Arc::clone(&self.projector),
            account_monitor: Arc::clone(&self.account_monitor),
            account_projector: Arc::clone(&self.account_projector),
        }
    }

    pub fn reconcile_state(&self) -> ReconcileState {
        ReconcileState {
            observation_service: Arc::clone(&self.observation_service),
            mutation_store: Arc::clone(&self.mutation_store),
            effect_store: Arc::clone(&self.effect_store),
            exchange_freshness: Arc::clone(&self.exchange_freshness),
            reconcile_guards: Arc::clone(&self.reconcile_guards),
            submit_preflight: Arc::clone(&self.submit_preflight),
        }
    }

    pub fn runtime_state(&self) -> RuntimeState {
        RuntimeState {
            reconcile: self.reconcile_state(),
            notifications: self.notifications.clone(),
            account_monitor: Arc::clone(&self.account_monitor),
            account_margin_guard: Arc::clone(&self.account_margin_guard),
        }
    }

    pub fn effect_worker_state(&self) -> EffectWorkerState {
        EffectWorkerState {
            reconcile: self.reconcile_state(),
            effect_service: Arc::clone(&self.effect_service),
            account_margin_guard: Arc::clone(&self.account_margin_guard),
        }
    }
}

#[cfg(test)]
impl From<ServerState> for HttpState {
    fn from(value: ServerState) -> Self {
        value.http_state()
    }
}

#[cfg(test)]
impl From<ServerState> for WebSocketState {
    fn from(value: ServerState) -> Self {
        value.websocket_state()
    }
}

#[cfg(test)]
impl From<ServerState> for RuntimeState {
    fn from(value: ServerState) -> Self {
        value.runtime_state()
    }
}

#[cfg(test)]
impl From<ServerState> for EffectWorkerState {
    fn from(value: ServerState) -> Self {
        value.effect_worker_state()
    }
}
