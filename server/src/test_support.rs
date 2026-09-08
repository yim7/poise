use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use poise_application::exchange::{
    AccountPort, AccountSummarySnapshot, ExchangeOpenOrderSnapshot, ExecutionPort, ExecutionQuote,
    OrderReceipt, OrderRequest, OrderStatus, Position, RecentTrackPnlRecords, UserDataEvent,
};
use poise_application::submit_effect_service::SubmitEffectService;
use poise_application::{
    AccountCapacityGuard, AccountMonitor, ApplicationNotification, EffectJournalDiagnostics,
    TrackEffectJournal, TrackMutationStore, TrackQueryStore, TrackRuntimeLifecycleService,
    TradingRuntime,
};
use poise_core::instrument::{Amount, Asset, InstrumentSpec};
use poise_core::risk::LossLimits;
use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
use poise_core::types::{Exposure, Side};
use poise_engine::execution_plan::TrackEffect;
use poise_engine::executor::SubmitRecoveryToken;
use poise_engine::manager::TrackManager;
use poise_engine::observation::MarketObservation;
use poise_engine::price_gate::SubmitPurpose;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use crate::exchange_freshness::ExchangeFreshness;
use crate::pnl_audit::RecentFillsAuditor;
use crate::projector::TrackProjector;
use crate::runtime::{
    AccountMarginGuardStore, PnlBackfillStatus, RecoveryAnomalyDirtyObserver, RecoveryDirtyState,
    RuntimeHealth, TrackReconcileGuards,
};
use crate::server_context::{EffectWorkerState, RuntimeState};

pub(crate) fn test_amount(value: f64, asset: &str) -> Amount {
    Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
}

pub(crate) fn test_account_summary_snapshot(
    valuation_asset: &str,
    equity: f64,
    available: Option<f64>,
    available_by_asset: &[(&str, f64)],
    unrealized_pnl: Option<f64>,
    observed_at: DateTime<Utc>,
) -> AccountSummarySnapshot {
    AccountSummarySnapshot::try_new(
        test_amount(equity, valuation_asset),
        available.map(|value| test_amount(value, valuation_asset)),
        available_by_asset
            .iter()
            .map(|(asset, value)| (Asset::try_new(*asset).unwrap(), *value))
            .collect(),
        unrealized_pnl.map(|value| test_amount(value, valuation_asset)),
        observed_at,
    )
    .unwrap()
}

#[derive(Clone)]
pub(crate) struct TestApplicationServices {
    pub(crate) runtime: Arc<TradingRuntime>,
    pub(crate) submit_effect_service: Arc<SubmitEffectService>,
    pub(crate) runtime_lifecycle_service: Arc<TrackRuntimeLifecycleService>,
    pub(crate) session_effect_queue: poise_application::SessionEffectQueue,
    pub(crate) effect_journal_diagnostics: Arc<EffectJournalDiagnostics>,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) account_margin_guard: Arc<AccountMarginGuardStore>,
    pub(crate) recovery_dirty_state: Arc<RecoveryDirtyState>,
}

#[derive(Clone)]
pub(crate) struct RuntimeTestContext {
    runtime_state: RuntimeState,
    pub(crate) notifications: broadcast::Sender<ApplicationNotification>,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
    runtime: Arc<TradingRuntime>,
}

impl RuntimeTestContext {
    pub(crate) fn runtime_state(&self) -> RuntimeState {
        self.runtime_state.clone()
    }

    pub(crate) async fn track_instruments(&self) -> Vec<poise_application::TrackInstrument> {
        self.runtime.track_instruments().await
    }

    pub(crate) async fn observe_market(
        &self,
        id: &str,
        reference_price: f64,
    ) -> Result<(), anyhow::Error> {
        self.runtime
            .handle_market_for_test(
                id,
                MarketObservation::MarkPrice {
                    mark_price: reference_price,
                },
            )
            .await?;
        self.runtime
            .handle_market_for_test(
                id,
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: reference_price,
                        best_ask: reference_price,
                    },
                },
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn seed_market_order_request(
        &self,
        id: &str,
        reference_price: f64,
    ) -> Result<OrderRequest, anyhow::Error> {
        self.observe_market(id, reference_price).await?;
        self.runtime
            .manager()
            .read()
            .await
            .get_track(id)
            .and_then(|state| state.first_executor_order_request())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("seed market did not create an order request"))
    }
}

#[derive(Clone)]
pub(crate) struct EffectWorkerTestContext {
    pub(crate) effect_worker_state: EffectWorkerState,
    pub(crate) exchange_freshness: Arc<ExchangeFreshness>,
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
    let runtime = Arc::new(TradingRuntime::new_with_recovery_anomaly_observer(
        manager,
        mutation_store,
        effect_store,
        notifications.clone(),
        account_margin_guard.clone() as Arc<dyn AccountCapacityGuard>,
        Arc::new(RecoveryAnomalyDirtyObserver::new(
            recovery_dirty_state.clone(),
        )),
    ));
    TestApplicationServices {
        runtime: Arc::clone(&runtime),
        submit_effect_service: Arc::new(SubmitEffectService::new(Arc::clone(&runtime))),
        runtime_lifecycle_service: Arc::new(TrackRuntimeLifecycleService::new(
            Arc::clone(&runtime),
            query_store,
        )),
        session_effect_queue: runtime.session_effect_queue(),
        effect_journal_diagnostics: runtime.effect_journal_diagnostics(),
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

pub(crate) fn test_track_definition(track_id: &str) -> TrackDefinition {
    test_track_definition_for(
        track_id,
        default_symbol_for(track_id),
        test_max_notional(),
        test_loss_limits(),
    )
}

fn test_track_definition_for(
    track_id: &str,
    symbol: &str,
    max_notional: f64,
    loss_limits: LossLimits,
) -> TrackDefinition {
    TrackDefinition::try_new(
        TrackId::new(track_id),
        Instrument::new(Venue::Binance, symbol.to_string()),
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        },
        Some(max_notional),
        loss_limits,
        Some(30),
    )
    .unwrap()
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
            TrackDefinition::try_new(
                TrackId::new(track_id),
                Instrument::new(Venue::Binance, default_symbol_for(track_id)),
                TrackConfig {
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: ShapeFamily::Linear,
                    out_of_band_policy: BandProtectionPolicy::Freeze,
                    risk_acquisition: Default::default(),
                },
                Some(test_max_notional()),
                test_loss_limits(),
                None,
            )
            .unwrap(),
            test_instrument_spec_for(Instrument::new(
                Venue::Binance,
                default_symbol_for(track_id),
            )),
        )
        .unwrap();
    manager
}

pub(crate) fn test_instrument_spec() -> InstrumentSpec {
    test_instrument_spec_for(Instrument::new(Venue::Binance, "BTCUSDT"))
}

pub(crate) fn test_instrument_spec_for(instrument: Instrument) -> InstrumentSpec {
    test_instrument_spec_with(instrument, 0.1, 0.1, 0.0, 0.0)
}

pub(crate) fn test_instrument_spec_with(
    instrument: Instrument,
    price_tick: f64,
    quantity_step: f64,
    min_quantity: f64,
    min_notional: f64,
) -> InstrumentSpec {
    let quote_asset = Asset::try_new("USDT").unwrap();
    let base_asset = instrument
        .symbol
        .strip_suffix("USDT")
        .filter(|value| !value.is_empty())
        .unwrap_or("BTC")
        .to_string();
    InstrumentSpec::try_linear(
        instrument,
        Asset::try_new(base_asset).unwrap(),
        quote_asset.clone(),
        quote_asset.clone(),
        Default::default(),
        price_tick,
        quantity_step,
        min_quantity,
        Amount::new(min_notional, quote_asset).unwrap(),
    )
    .unwrap()
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
            quantity: poise_core::instrument::PositionQty::new(0.1).unwrap(),
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
        .save_track_control_state(
            &TrackId::new(track_id),
            &poise_application::TrackControlState::default(),
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
    async fn submit_order(
        &self,
        req: OrderRequest,
    ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
        self.submitted.lock().unwrap().push(req.clone());
        Ok(OrderReceipt {
            order_id: "test-order".into(),
            client_order_id: req.client_order_id,
            filled_qty: poise_core::instrument::PositionQty::default(),
            status: OrderStatus::New,
        })
    }

    async fn cancel_order(
        &self,
        _instrument: &poise_core::track::Instrument,
        order_id: &str,
    ) -> poise_application::exchange::ExecutionResult<OrderReceipt> {
        Ok(OrderReceipt {
            order_id: order_id.to_string(),
            client_order_id: String::new(),
            filled_qty: poise_core::instrument::PositionQty::default(),
            status: OrderStatus::Canceled,
        })
    }

    async fn cancel_all(
        &self,
        _instrument: &poise_core::track::Instrument,
    ) -> poise_application::exchange::ExecutionResult<()> {
        self.cancel_all_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn get_position(
        &self,
        instrument: &poise_core::track::Instrument,
    ) -> poise_application::exchange::ExecutionResult<Position> {
        Ok(Position {
            instrument: instrument.clone(),
            qty: poise_core::instrument::PositionQty::default(),
            avg_price: 0.0,
            unrealized_pnl: 0.0,
            mark_price: None,
        })
    }

    async fn get_open_orders(
        &self,
        _instrument: &poise_core::track::Instrument,
    ) -> poise_application::exchange::ExecutionResult<ExchangeOpenOrderSnapshot> {
        Ok(ExchangeOpenOrderSnapshot::from_complete_exchange_query(
            Vec::new(),
        ))
    }
}

#[derive(Default)]
pub(crate) struct NoopAccountPort;

#[async_trait::async_trait]
impl AccountPort for NoopAccountPort {
    async fn get_recent_track_pnl_records(
        &self,
        _instrument: &poise_core::track::Instrument,
        required_window: &poise_application::exchange::PnlHistoryWindow,
    ) -> Result<RecentTrackPnlRecords> {
        Ok(RecentTrackPnlRecords::Available(
            poise_application::exchange::RecentTrackPnlBatch::complete(
                required_window.clone(),
                Vec::new(),
            ),
        ))
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        let (_sender, receiver) = mpsc::channel(1);
        Ok(receiver)
    }
}

pub(crate) fn build_http_state(
    services: &TestApplicationServices,
    query_store: Arc<dyn TrackQueryStore>,
    query_service: Arc<poise_application::TradingQueries>,
    debug_query_service: Arc<poise_application::TrackDebugQueryService>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<crate::account_projector::AccountProjector>,
) -> crate::server_context::HttpState {
    crate::assembly::build_http_state(
        Arc::clone(&services.runtime),
        query_service,
        debug_query_service,
        projector,
        account_monitor,
        account_projector,
        Arc::new(RuntimeHealth::new()),
        Arc::new(PnlBackfillStatus::new()),
        services.effect_journal_diagnostics.clone(),
        Arc::new(ExchangeFreshness::default()),
        services.account_margin_guard.clone(),
        Arc::new(RecentFillsAuditor::new(
            Arc::new(NoopAccountPort),
            query_store,
        )),
        Arc::new(std::collections::HashMap::from([(
            "btc-core".to_string(),
            crate::server_context::TrackConfigurationMetadata {
                leverage: 10,
                tick_timeout_secs: 60,
            },
        )])),
        None,
    )
}

pub(crate) fn build_websocket_state(
    services: &TestApplicationServices,
    query_service: Arc<poise_application::TradingQueries>,
    projector: Arc<TrackProjector>,
    account_monitor: Arc<AccountMonitor>,
    account_projector: Arc<crate::account_projector::AccountProjector>,
) -> crate::server_context::WebSocketState {
    crate::assembly::build_websocket_state(
        services.notifications.clone(),
        broadcast::channel(1024).0,
        Arc::clone(&services.runtime),
        query_service,
        projector,
        account_monitor,
        account_projector,
    )
}

pub(crate) fn build_runtime_and_effect_worker_test_contexts(
    services: &TestApplicationServices,
    query_store: Arc<dyn TrackQueryStore>,
    _effect_store: Arc<dyn TrackEffectJournal>,
    account_monitor: Arc<AccountMonitor>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let reconcile_guards = Arc::new(TrackReconcileGuards::default());
    let runtime_health = Arc::new(RuntimeHealth::new());
    let pnl_backfill_status = Arc::new(PnlBackfillStatus::new());
    let recent_fills_auditor = Arc::new(RecentFillsAuditor::new(
        Arc::new(NoopAccountPort),
        query_store,
    ));
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.runtime),
        Arc::clone(&services.runtime_lifecycle_service),
        exchange_freshness.clone(),
        reconcile_guards,
        Arc::clone(&services.recovery_dirty_state),
    );
    let runtime_state = crate::assembly::build_runtime_state(
        reconcile.clone(),
        broadcast::channel(1024).0,
        Arc::clone(&account_monitor),
        Arc::clone(&services.account_margin_guard),
        runtime_health.clone(),
        pnl_backfill_status,
        recent_fills_auditor,
    );
    let effect_worker_state = crate::assembly::build_effect_worker_state(
        reconcile,
        Arc::clone(&services.submit_effect_service),
        Arc::clone(&services.account_margin_guard),
        services.session_effect_queue.clone(),
        runtime_health,
    );
    build_test_contexts_from_runtime_states(
        runtime_state,
        effect_worker_state,
        services.notifications.clone(),
        Arc::clone(&services.runtime),
    )
}

pub(crate) fn build_test_contexts_from_runtime_states(
    runtime_state: RuntimeState,
    effect_worker_state: EffectWorkerState,
    notifications: broadcast::Sender<ApplicationNotification>,
    runtime: Arc<TradingRuntime>,
) -> (RuntimeTestContext, EffectWorkerTestContext) {
    debug_assert!(Arc::ptr_eq(
        &runtime_state.reconcile.exchange_freshness,
        &effect_worker_state.reconcile.exchange_freshness,
    ));
    debug_assert!(Arc::ptr_eq(
        &runtime_state.runtime_health,
        &effect_worker_state.runtime_health,
    ));

    let exchange_freshness = Arc::clone(&runtime_state.reconcile.exchange_freshness);

    (
        RuntimeTestContext {
            runtime_state,
            notifications,
            exchange_freshness: Arc::clone(&exchange_freshness),
            runtime: Arc::clone(&runtime),
        },
        EffectWorkerTestContext {
            effect_worker_state,
            exchange_freshness,
        },
    )
}

pub(crate) fn build_effect_worker_test_context(
    services: &TestApplicationServices,
    _query_store: Arc<dyn TrackQueryStore>,
    _effect_store: Arc<dyn TrackEffectJournal>,
) -> EffectWorkerTestContext {
    let exchange_freshness = Arc::new(ExchangeFreshness::default());
    let runtime_health = Arc::new(RuntimeHealth::new());
    let reconcile = crate::assembly::build_reconcile_state(
        Arc::clone(&services.runtime),
        Arc::clone(&services.runtime_lifecycle_service),
        exchange_freshness.clone(),
        Arc::new(TrackReconcileGuards::default()),
        Arc::clone(&services.recovery_dirty_state),
    );
    EffectWorkerTestContext {
        effect_worker_state: crate::assembly::build_effect_worker_state(
            reconcile,
            Arc::clone(&services.submit_effect_service),
            Arc::clone(&services.account_margin_guard),
            services.session_effect_queue.clone(),
            runtime_health,
        ),
        exchange_freshness,
    }
}
