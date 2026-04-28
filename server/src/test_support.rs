use std::sync::Arc;

use anyhow::Result;
use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountCapacityGuard, AccountMonitor, ApplicationNotification, ConfiguredTrackDefinition,
    ConfiguredTrackInput, PreparedTrackRegistry, TrackCommandService, TrackEffectJournal,
    TrackEffectService, TrackMutationStore, TrackObservationService, TrackQueryStore,
    TrackRuntimeLifecycleService, TrackServiceSet,
};
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, ShapeFamily};
use poise_core::track::{TrackId, Venue};
use poise_core::types::{ExchangeRules, Exposure, Side};
use poise_engine::execution_plan::TrackEffect;
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::manager::TrackManager;
use poise_engine::observation::MarketObservation;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, ExchangeOpenOrderSnapshot, ExecutionPort, ExecutionQuote,
    OrderReceipt, OrderRequest, OrderStatus, Position, UserDataEvent,
};
use poise_engine::price_gate::SubmitPurpose;
use poise_engine::transition::TrackTransition;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

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
    pub(crate) runtime_lifecycle_service: Arc<TrackRuntimeLifecycleService>,
    pub(crate) session_effect_queue: poise_application::SessionEffectQueue,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) account_margin_guard: Arc<AccountMarginGuardStore>,
    pub(crate) recovery_dirty_state: Arc<RecoveryDirtyState>,
}

#[derive(Clone)]
pub(crate) struct RuntimeTestContext {
    runtime_state: RuntimeState,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    pub(crate) submit_preflight: Arc<SubmitPreflight>,
    observation_service: Arc<TrackObservationService>,
}

impl RuntimeTestContext {
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
                MarketObservation::MarkPrice {
                    mark_price: reference_price,
                },
            )
            .await?;
        self.observation_service
            .observe_market(
                id,
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: reference_price,
                        best_ask: reference_price,
                    },
                },
            )
            .await
    }
}

#[derive(Clone)]
pub(crate) struct EffectWorkerTestContext {
    pub(crate) effect_worker_state: EffectWorkerState,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    pub(crate) submit_preflight: Arc<SubmitPreflight>,
}

impl From<EffectWorkerTestContext> for EffectWorkerState {
    fn from(value: EffectWorkerTestContext) -> Self {
        value.effect_worker_state
    }
}

pub(crate) fn build_test_application_services(
    manager: TrackManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectJournal>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
) -> TestApplicationServices {
    let recovery_dirty_state = Arc::new(RecoveryDirtyState::default());
    build_test_application_services_with_recovery_dirty_state(
        manager,
        mutation_store,
        query_store,
        effect_store,
        notifications,
        account_margin_guard,
        recovery_dirty_state,
    )
}

pub(crate) fn build_test_application_services_with_recovery_dirty_state(
    manager: TrackManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectJournal>,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
    recovery_dirty_state: Arc<RecoveryDirtyState>,
) -> TestApplicationServices {
    let services = TrackServiceSet::new_with_recovery_anomaly_observer(
        manager,
        mutation_store,
        query_store,
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
        runtime_lifecycle_service: Arc::new(services.runtime_lifecycle),
        session_effect_queue: services.session_effect_queue,
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

pub(crate) fn test_max_notional() -> f64 {
    3000.0
}

pub(crate) fn test_loss_limits() -> LossLimits {
    LossLimits {
        daily_loss_limit: 100.0,
        total_loss_limit: 300.0,
    }
}

pub(crate) fn test_prepared_registry(track_id: &str) -> Arc<PreparedTrackRegistry> {
    prepared_registry_for(
        track_id,
        default_symbol_for(track_id),
        test_max_notional(),
        test_loss_limits(),
    )
}

fn prepared_registry_for(
    track_id: &str,
    symbol: &str,
    max_notional: f64,
    loss_limits: LossLimits,
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
                out_of_band_policy: Some(BandProtectionPolicy::Freeze),
                max_notional: Some(max_notional),
                daily_loss_limit: loss_limits.daily_loss_limit,
                total_loss_limit: loss_limits.total_loss_limit,
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

pub(crate) fn test_manager(track_id: &str) -> TrackManager {
    let mut manager = TrackManager::new(Arc::new(crate::assembly::SystemClock));
    manager
        .add_track(
            TrackId::new(track_id),
            poise_core::track::Instrument::new(Venue::Binance, default_symbol_for(track_id)),
            poise_core::strategy::TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
            },
            test_max_notional(),
            test_loss_limits(),
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
        )
        .unwrap();
    manager
}

pub(crate) fn test_submit_effect(track_id: &str) -> TrackEffect {
    TrackEffect::SubmitOrder {
        request: OrderRequest {
            instrument: poise_core::track::Instrument::new(
                Venue::Binance,
                default_symbol_for(track_id),
            ),
            side: Side::Buy,
            price: 100.0,
            quantity: 0.1,
            client_order_id: format!("{track_id}-old-session-submit"),
            reduce_only: false,
        },
        desired_exposure: Exposure(4.0),
        submit_purpose: SubmitPurpose::AutoReconcile,
        recovery_token: SubmitRecoveryToken::empty(),
    }
}

pub(crate) async fn seed_persisted_pending_submit_effect<R>(
    store: &R,
    track_id: &str,
) -> Result<String>
where
    R: TrackMutationStore + TrackEffectJournal + ?Sized,
{
    store
        .commit_track_transition(
            track_id,
            None,
            &poise_engine::ledger::TrackLedgerState::default(),
            &[],
        )
        .await?;
    let created_at = chrono::Utc::now();
    let entries = vec![poise_application::EffectJournalEntry {
        effect_id: format!("{}:batch:{}:0", track_id, created_at.timestamp_micros()),
        track_id: TrackId::new(track_id),
        batch_id: format!("{}:batch:{}", track_id, created_at.timestamp_micros()),
        sequence: 0,
        effect: test_submit_effect(track_id),
        created_at,
    }];
    store.append_entries(&entries).await?;
    entries
        .first()
        .map(|effect| effect.effect_id.clone())
        .ok_or_else(|| anyhow::anyhow!("seeded transition did not create an effect journal entry"))
}

pub(crate) fn build_effect_worker_context_for_repository<R>(
    repository: Arc<R>,
) -> EffectWorkerTestContext
where
    R: TrackMutationStore + TrackQueryStore + TrackEffectJournal + 'static,
{
    let (notifications, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
    let query_store = repository.clone() as Arc<dyn TrackQueryStore>;
    let effect_store = repository as Arc<dyn TrackEffectJournal>;
    let services = build_test_application_services(
        test_manager("btc-core"),
        mutation_store,
        query_store.clone(),
        effect_store.clone(),
        notifications,
        account_margin_guard,
    );
    build_effect_worker_test_context(&services, query_store, effect_store)
}

#[derive(Default)]
pub(crate) struct RecordingExecutionPort {
    submitted: std::sync::Mutex<Vec<OrderRequest>>,
    cancel_all_count: std::sync::atomic::AtomicUsize,
}

impl RecordingExecutionPort {
    pub(crate) fn submit_order_call_count(&self) -> usize {
        self.submitted.lock().unwrap().len()
    }

    pub(crate) fn cancel_all_call_count(&self) -> usize {
        self.cancel_all_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ExecutionPort for RecordingExecutionPort {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.submitted.lock().unwrap().push(req.clone());
        Ok(OrderReceipt {
            order_id: "test-order".into(),
            client_order_id: req.client_order_id,
            filled_qty: 0.0,
            status: OrderStatus::New,
        })
    }

    async fn cancel_order(
        &self,
        _instrument: &poise_core::track::Instrument,
        order_id: &str,
    ) -> Result<OrderReceipt> {
        Ok(OrderReceipt {
            order_id: order_id.to_string(),
            client_order_id: String::new(),
            filled_qty: 0.0,
            status: OrderStatus::Canceled,
        })
    }

    async fn cancel_all(&self, _instrument: &poise_core::track::Instrument) -> Result<()> {
        self.cancel_all_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn get_position(&self, instrument: &poise_core::track::Instrument) -> Result<Position> {
        Ok(Position {
            instrument: instrument.clone(),
            qty: 0.0,
            avg_price: 0.0,
            unrealized_pnl: 0.0,
        })
    }

    async fn get_open_orders(
        &self,
        _instrument: &poise_core::track::Instrument,
    ) -> Result<ExchangeOpenOrderSnapshot> {
        Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
            Vec::new(),
        ))
    }
}

#[derive(Default)]
pub(crate) struct NoopAccountPort;

#[async_trait::async_trait]
impl AccountPort for NoopAccountPort {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &poise_core::track::Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        Ok(AccountCapacitySnapshot {
            max_increase_notional: 1_000_000.0,
        })
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (_sender, receiver) = mpsc::channel(1);
        Ok(receiver)
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

pub(crate) fn build_runtime_and_effect_worker_test_contexts(
    services: &TestApplicationServices,
    _query_store: Arc<dyn TrackQueryStore>,
    _effect_store: Arc<dyn TrackEffectJournal>,
    account_monitor: Arc<AccountMonitor>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile_guards = Arc::new(TrackReconcileGuards::default());
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        Arc::clone(&services.runtime_lifecycle_service),
        services.session_effect_queue.clone(),
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
        services.session_effect_queue.clone(),
    );
    build_test_contexts_from_runtime_states(
        runtime_state,
        effect_worker_state,
        services.notifications.clone(),
        Arc::clone(&services.observation_service),
    )
}

pub(crate) fn build_test_contexts_from_runtime_states(
    runtime_state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    notifications: broadcast::Sender<ApplicationNotification>,
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

    (
        RuntimeTestContext {
            runtime_state,
            notifications,
            exchange_freshness: Arc::clone(&exchange_freshness),
            submit_preflight: Arc::clone(&submit_preflight),
            observation_service: Arc::clone(&observation_service),
        },
        EffectWorkerTestContext {
            effect_worker_state,
            exchange_freshness,
            submit_preflight,
        },
    )
}

pub(crate) fn build_effect_worker_test_context(
    services: &TestApplicationServices,
    _query_store: Arc<dyn TrackQueryStore>,
    _effect_store: Arc<dyn TrackEffectJournal>,
) -> EffectWorkerTestContext {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let submit_preflight = Arc::new(SubmitPreflight::new());
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.observation_service),
        Arc::clone(&services.runtime_lifecycle_service),
        services.session_effect_queue.clone(),
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
            services.session_effect_queue.clone(),
        ),
        exchange_freshness,
        submit_preflight,
    }
}
