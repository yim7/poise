#![cfg(test)]

use std::sync::Arc;

use poise_application::{
    AccountCapacityGuard, AccountMonitor, ApplicationNotification, TrackBudgetCatalog,
    TrackCommandService, TrackEffectService, TrackEffectStore, TrackMutationStore,
    TrackObservationService, TrackServiceSet,
};
use poise_core::risk::CapacityBudget;
use poise_engine::command::TrackCommand;
use poise_engine::executor::OrderUpdateAbsorbResult;
use poise_engine::manager::TrackManager;
use poise_engine::observation::{OrderObservation, PositionObservation};
use poise_engine::track::TrackId;
use poise_engine::transition::TrackTransition;
use tokio::sync::RwLock;
use tokio::sync::broadcast;

use crate::exchange_freshness::ExchangeFreshness;
use crate::projector::TrackProjector;
use crate::runtime::AccountMarginGuardStore;
use crate::runtime::TrackReconcileGuards;
use crate::server_context::{EffectWorkerState, RuntimeState};
use crate::submit_preflight::SubmitPreflight;

#[derive(Clone)]
pub(crate) struct TestApplicationServices {
    pub(crate) command_service: Arc<TrackCommandService>,
    pub(crate) observation_service: Arc<TrackObservationService>,
    pub(crate) effect_service: Arc<TrackEffectService>,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) account_margin_guard: Arc<AccountMarginGuardStore>,
}

#[derive(Clone)]
pub(crate) struct RuntimeTestContext {
    runtime_state: RuntimeState,
    manager: Arc<RwLock<TrackManager>>,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    pub(crate) submit_preflight: Arc<SubmitPreflight>,
    pub(crate) account_margin_guard: Arc<AccountMarginGuardStore>,
    pub(crate) projector: Arc<TrackProjector>,
    command_service: Arc<TrackCommandService>,
    observation_service: Arc<TrackObservationService>,
}

impl RuntimeTestContext {
    pub(crate) fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.manager.clone()
    }

    pub(crate) fn runtime_state(&self) -> RuntimeState {
        self.runtime_state.clone()
    }

    pub(crate) async fn track_instruments(&self) -> Vec<poise_application::TrackInstrument> {
        self.observation_service.track_instruments().await
    }

    pub(crate) async fn observe_market(
        &self,
        id: &str,
        reference_price: f64,
    ) -> Result<TrackTransition, anyhow::Error> {
        self.observation_service
            .observe_market(id, reference_price)
            .await
    }

    pub(crate) async fn observe_position(
        &self,
        id: &str,
        observation: PositionObservation,
    ) -> Result<TrackTransition, anyhow::Error> {
        self.observation_service
            .observe_position(id, observation)
            .await
    }

    pub(crate) async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, OrderUpdateAbsorbResult), anyhow::Error> {
        self.observation_service
            .observe_order_with_absorb_result(id, observation)
            .await
    }

    pub(crate) async fn command(
        &self,
        id: &str,
        command: TrackCommand,
    ) -> Result<TrackTransition, anyhow::Error> {
        self.command_service.command(id, command).await
    }
}

#[derive(Clone)]
pub(crate) struct EffectWorkerTestContext {
    pub(crate) effect_worker_state: EffectWorkerState,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    pub(crate) submit_preflight: Arc<SubmitPreflight>,
    manager: Arc<RwLock<TrackManager>>,
    observation_service: Arc<TrackObservationService>,
}

impl EffectWorkerTestContext {
    pub(crate) fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.manager.clone()
    }

    pub(crate) async fn observe_market(
        &self,
        id: &str,
        reference_price: f64,
    ) -> Result<TrackTransition, anyhow::Error> {
        self.observation_service
            .observe_market(id, reference_price)
            .await
    }

    pub(crate) async fn observe_order_with_absorb_result(
        &self,
        id: &str,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, OrderUpdateAbsorbResult), anyhow::Error> {
        self.observation_service
            .observe_order_with_absorb_result(id, observation)
            .await
    }
}

impl From<EffectWorkerTestContext> for EffectWorkerState {
    fn from(value: EffectWorkerTestContext) -> Self {
        value.effect_worker_state
    }
}

pub(crate) fn build_test_application_services(
    manager: TrackManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
) -> TestApplicationServices {
    let services = TrackServiceSet::new(
        manager,
        mutation_store,
        effect_store,
        notifications.clone(),
        account_margin_guard.clone() as Arc<dyn AccountCapacityGuard>,
    );
    TestApplicationServices {
        command_service: Arc::new(services.command),
        observation_service: Arc::new(services.observation),
        effect_service: Arc::new(services.effect),
        notifications,
        account_margin_guard,
    }
}

pub(crate) fn unavailable_account_monitor(
    notifications: broadcast::Sender<ApplicationNotification>,
) -> Arc<AccountMonitor> {
    Arc::new(AccountMonitor::unavailable(
        notifications,
        poise_application::AccountMonitorConfig::default(),
    ))
}

pub(crate) fn test_budget() -> CapacityBudget {
    CapacityBudget {
        max_notional: 3000.0,
        daily_loss_limit: 100.0,
        total_loss_limit: 300.0,
    }
}

pub(crate) fn test_budget_catalog(track_id: &str) -> TrackBudgetCatalog {
    budget_catalog_for(track_id, test_budget())
}

pub(crate) fn budget_catalog_for(track_id: &str, budget: CapacityBudget) -> TrackBudgetCatalog {
    TrackBudgetCatalog::from_iter([(TrackId::new(track_id), budget)])
}

pub(crate) fn build_http_state(
    services: &TestApplicationServices,
    query_service: Arc<poise_application::TrackQueryService>,
    debug_query_service: Arc<poise_application::TrackDebugQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<crate::account_projector::AccountProjector>,
) -> crate::server_context::HttpState {
    crate::assembly::build_http_state(
        Arc::clone(&services.command_service),
        query_service,
        debug_query_service,
        projector,
        account_monitor,
        account_projector,
    )
}

pub(crate) fn build_websocket_state(
    services: &TestApplicationServices,
    query_service: Arc<poise_application::TrackQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<crate::account_projector::AccountProjector>,
) -> crate::server_context::WebSocketState {
    crate::assembly::build_websocket_state(
        services.notifications.clone(),
        query_service,
        projector,
        account_monitor,
        account_projector,
    )
}

pub(crate) fn build_runtime_test_context(
    services: &TestApplicationServices,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    account_monitor: Arc<AccountMonitor>,
    projector: Arc<TrackProjector>,
) -> RuntimeTestContext {
    build_runtime_and_effect_worker_test_contexts(
        services,
        mutation_store,
        effect_store,
        account_monitor,
        projector,
    )
    .0
}

pub(crate) fn build_runtime_and_effect_worker_test_contexts(
    services: &TestApplicationServices,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    account_monitor: Arc<AccountMonitor>,
    projector: Arc<TrackProjector>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile_guards = Arc::new(TrackReconcileGuards::default());
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        mutation_store,
        effect_store,
        exchange_freshness.clone(),
        reconcile_guards,
        submit_preflight.clone(),
    );
    let runtime_state = crate::assembly::build_runtime_state(
        reconcile.clone(),
        services.notifications.clone(),
        Arc::clone(&account_monitor),
        Arc::clone(&services.account_margin_guard),
    );
    let effect_worker_state = crate::assembly::build_effect_worker_state(
        reconcile,
        Arc::clone(&services.effect_service),
        Arc::clone(&services.account_margin_guard),
    );
    build_test_contexts_from_runtime_states(
        runtime_state,
        effect_worker_state,
        services.observation_service.manager(),
        services.notifications.clone(),
        projector,
        Arc::clone(&services.command_service),
        Arc::clone(&services.observation_service),
    )
}

pub(crate) fn build_test_contexts_from_runtime_states(
    runtime_state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    manager: Arc<RwLock<TrackManager>>,
    notifications: broadcast::Sender<ApplicationNotification>,
    projector: Arc<TrackProjector>,
    command_service: Arc<TrackCommandService>,
    observation_service: Arc<TrackObservationService>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    debug_assert!(Arc::ptr_eq(
        &runtime_state.reconcile.exchange_freshness,
        &effect_worker_state.reconcile.exchange_freshness,
    ));
    debug_assert!(Arc::ptr_eq(
        &runtime_state.reconcile.submit_preflight,
        &effect_worker_state.reconcile.submit_preflight,
    ));

    let exchange_freshness = Arc::clone(&runtime_state.reconcile.exchange_freshness);
    let submit_preflight = Arc::clone(&runtime_state.reconcile.submit_preflight);
    let account_margin_guard = Arc::clone(&runtime_state.account_margin_guard);

    (
        RuntimeTestContext {
            runtime_state,
            manager: Arc::clone(&manager),
            notifications,
            exchange_freshness: Arc::clone(&exchange_freshness),
            submit_preflight: Arc::clone(&submit_preflight),
            account_margin_guard,
            projector,
            command_service,
            observation_service: Arc::clone(&observation_service),
        },
        EffectWorkerTestContext {
            effect_worker_state,
            exchange_freshness,
            submit_preflight,
            manager,
            observation_service,
        },
    )
}

pub(crate) fn build_effect_worker_test_context(
    services: &TestApplicationServices,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
) -> EffectWorkerTestContext {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        mutation_store,
        effect_store,
        exchange_freshness.clone(),
        Arc::new(TrackReconcileGuards::default()),
        submit_preflight.clone(),
    );
    EffectWorkerTestContext {
        effect_worker_state: crate::assembly::build_effect_worker_state(
            reconcile,
            Arc::clone(&services.effect_service),
            Arc::clone(&services.account_margin_guard),
        ),
        exchange_freshness,
        submit_preflight,
        manager: services.observation_service.manager(),
        observation_service: Arc::clone(&services.observation_service),
    }
}
