use std::sync::Arc;

use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountCapacityGuard, AccountMonitor, ApplicationNotification, ConfiguredTrackDefinition,
    ConfiguredTrackInput, PreparedTrackRegistry, TrackCommandService, TrackEffectService,
    TrackEffectStore, TrackMutationStore, TrackObservationService, TrackQueryService,
    TrackQueryStore, TrackServiceSet,
};
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily};
use poise_engine::command::TrackCommand;
use poise_engine::executor::OrderUpdateAbsorbResult;
use poise_engine::manager::TrackManager;
use poise_engine::observation::{MarketObservation, OrderObservation, PositionObservation};
use poise_engine::ports::ExecutionQuote;
use poise_engine::track::{TrackId, Venue};
use poise_engine::transition::TrackTransition;
use tokio::sync::RwLock;
use tokio::sync::broadcast;

use crate::exchange_freshness::ExchangeFreshness;
use crate::projector::TrackProjector;
use crate::runtime::{
    AccountMarginGuardStore, RecoveryAnomalyDirtyObserver, RecoveryDirtyState, TrackReconcileGuards,
};
use crate::server_context::{EffectWorkerState, RuntimeState};
use crate::submit_preflight::SubmitPreflight;

#[derive(Clone)]
pub(crate) struct TestApplicationServices {
    pub(crate) command_service: Arc<TrackCommandService>,
    pub(crate) observation_service: Arc<TrackObservationService>,
    pub(crate) effect_service: Arc<TrackEffectService>,
    pub(crate) submit_effect_service: Arc<SubmitEffectService>,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) account_margin_guard: Arc<AccountMarginGuardStore>,
    pub(crate) recovery_dirty_state: Arc<RecoveryDirtyState>,
}

#[derive(Clone)]
pub(crate) struct RuntimeTestContext {
    runtime_state: RuntimeState,
    manager: Arc<RwLock<TrackManager>>,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    pub(crate) submit_preflight: Arc<SubmitPreflight>,
    pub(crate) recovery_dirty_state: Arc<RecoveryDirtyState>,
    pub(crate) effect_service: Arc<TrackEffectService>,
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
            .observe_market(
                id,
                MarketObservation {
                    mark_price: reference_price,
                    execution_quote: Some(ExecutionQuote {
                        best_bid: reference_price,
                        best_ask: reference_price,
                    }),
                },
            )
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
            .observe_market(
                id,
                MarketObservation {
                    mark_price: reference_price,
                    execution_quote: Some(ExecutionQuote {
                        best_bid: reference_price,
                        best_ask: reference_price,
                    }),
                },
            )
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
    let recovery_dirty_state = Arc::new(RecoveryDirtyState::default());
    build_test_application_services_with_recovery_dirty_state(
        manager,
        mutation_store,
        effect_store,
        notifications,
        account_margin_guard,
        recovery_dirty_state,
    )
}

pub(crate) fn build_test_application_services_with_recovery_dirty_state(
    manager: TrackManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
    recovery_dirty_state: Arc<RecoveryDirtyState>,
) -> TestApplicationServices {
    let services = TrackServiceSet::new_with_recovery_anomaly_observer(
        manager,
        mutation_store,
        effect_store,
        notifications.clone(),
        account_margin_guard.clone() as Arc<dyn AccountCapacityGuard>,
        Arc::new(RecoveryAnomalyDirtyObserver::new(
            recovery_dirty_state.clone(),
        )),
    );
    TestApplicationServices {
        command_service: Arc::new(services.command),
        observation_service: Arc::new(services.observation),
        effect_service: Arc::new(services.effect),
        submit_effect_service: Arc::new(services.submit_effect),
        notifications,
        account_margin_guard,
        recovery_dirty_state,
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

pub(crate) fn test_prepared_registry(track_id: &str) -> Arc<PreparedTrackRegistry> {
    prepared_registry_for(track_id, default_symbol_for(track_id), test_budget())
}

fn prepared_registry_for(
    track_id: &str,
    symbol: &str,
    budget: CapacityBudget,
) -> Arc<PreparedTrackRegistry> {
    Arc::new(
        PreparedTrackRegistry::new(vec![
            ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
                track_id: TrackId::new(track_id),
                venue: Venue::Binance,
                symbol: symbol.to_string(),
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(ShapeFamily::Linear),
                out_of_band_policy: Some(BandProtectionPolicy::Freeze {
                    recover: BandRecoverPolicy::BackInBand,
                }),
                max_notional: Some(budget.max_notional),
                daily_loss_limit: budget.daily_loss_limit,
                total_loss_limit: budget.total_loss_limit,
                tick_timeout_secs: Some(30),
            })
            .unwrap(),
        ])
        .unwrap(),
    )
}

fn default_symbol_for(track_id: &str) -> &str {
    match track_id {
        "btc-core" | "BTCUSDT" => "BTCUSDT",
        "eth-core" | "ETHUSDT" => "ETHUSDT",
        _ => track_id,
    }
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
        broadcast::channel(1024).0,
        Arc::clone(&services.observation_service),
        query_service,
        projector,
        account_monitor,
        account_projector,
    )
}

pub(crate) fn build_runtime_test_context(
    services: &TestApplicationServices,
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    account_monitor: Arc<AccountMonitor>,
    projector: Arc<TrackProjector>,
) -> RuntimeTestContext {
    build_runtime_and_effect_worker_test_contexts(
        services,
        query_store,
        effect_store,
        account_monitor,
        projector,
    )
    .0
}

pub(crate) fn build_runtime_and_effect_worker_test_contexts(
    services: &TestApplicationServices,
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectStore>,
    account_monitor: Arc<AccountMonitor>,
    projector: Arc<TrackProjector>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile_guards = Arc::new(TrackReconcileGuards::default());
    let query_service = Arc::new(TrackQueryService::new_with_observation(
        query_store,
        test_prepared_registry("BTCUSDT"),
        Some(Arc::clone(&services.observation_service)),
    ));
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        query_service,
        effect_store,
        exchange_freshness.clone(),
        reconcile_guards,
        submit_preflight.clone(),
        Arc::clone(&services.recovery_dirty_state),
    );
    let runtime_state = crate::assembly::build_runtime_state(
        reconcile.clone(),
        services.notifications.clone(),
        broadcast::channel(1024).0,
        Arc::clone(&account_monitor),
        Arc::clone(&services.account_margin_guard),
    );
    let effect_worker_state = crate::assembly::build_effect_worker_state(
        reconcile,
        Arc::clone(&services.effect_service),
        Arc::clone(&services.submit_effect_service),
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
        Arc::clone(&services.effect_service),
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
    effect_service: Arc<TrackEffectService>,
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
    let recovery_dirty_state = Arc::clone(&runtime_state.reconcile.recovery_dirty_state);
    let account_margin_guard = Arc::clone(&runtime_state.account_margin_guard);

    (
        RuntimeTestContext {
            runtime_state,
            manager: Arc::clone(&manager),
            notifications,
            exchange_freshness: Arc::clone(&exchange_freshness),
            submit_preflight: Arc::clone(&submit_preflight),
            recovery_dirty_state: Arc::clone(&recovery_dirty_state),
            effect_service,
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
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectStore>,
) -> EffectWorkerTestContext {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let query_service = Arc::new(TrackQueryService::new_with_observation(
        query_store,
        test_prepared_registry("BTCUSDT"),
        Some(Arc::clone(&services.observation_service)),
    ));
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        query_service,
        effect_store,
        exchange_freshness.clone(),
        Arc::new(TrackReconcileGuards::default()),
        submit_preflight.clone(),
        Arc::clone(&services.recovery_dirty_state),
    );
    EffectWorkerTestContext {
        effect_worker_state: crate::assembly::build_effect_worker_state(
            reconcile,
            Arc::clone(&services.effect_service),
            Arc::clone(&services.submit_effect_service),
            Arc::clone(&services.account_margin_guard),
        ),
        exchange_freshness,
        submit_preflight,
        manager: services.observation_service.manager(),
        observation_service: Arc::clone(&services.observation_service),
    }
}
