use super::*;

pub(crate) struct RuntimeFixture {
    pub(crate) runtime: ServerRuntime,
    pub(crate) state: RuntimeTestContext,
    pub(crate) worker: EffectWorkerTestContext,
    pub(crate) exchange: Arc<FakeExchange>,
    pub(crate) persistence: Arc<MemoryPersistence>,
    pub(crate) price_sender: mpsc::Sender<PriceTick>,
    pub(crate) user_sender: mpsc::Sender<UserDataEvent>,
}

pub(crate) struct RuntimeFixtureOptions {
    pub(crate) recovery_retry_interval: Duration,
    pub(crate) audit_interval: Duration,
    pub(crate) account_refresh_interval: Duration,
    pub(crate) clock: Arc<dyn ClockPort>,
}

pub(crate) async fn runtime_fixture(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
) -> RuntimeFixture {
    runtime_fixture_with_intervals(
        restored_snapshot,
        position,
        open_orders,
        budget,
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
}

pub(crate) async fn runtime_fixture_with_recovery_retry_interval(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    recovery_retry_interval: Duration,
) -> RuntimeFixture {
    runtime_fixture_with_intervals(
        restored_snapshot,
        position,
        open_orders,
        budget,
        recovery_retry_interval,
        Duration::from_secs(5),
    )
    .await
}

pub(crate) async fn runtime_fixture_with_account_refresh_interval(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    account_refresh_interval: Duration,
) -> RuntimeFixture {
    runtime_fixture_with_intervals_and_account_refresh(
        restored_snapshot,
        position,
        open_orders,
        budget,
        Duration::from_secs(1),
        Duration::from_secs(5),
        account_refresh_interval,
    )
    .await
}

pub(crate) async fn runtime_fixture_with_intervals(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    recovery_retry_interval: Duration,
    audit_interval: Duration,
) -> RuntimeFixture {
    runtime_fixture_with_intervals_and_account_refresh(
        restored_snapshot,
        position,
        open_orders,
        budget,
        recovery_retry_interval,
        audit_interval,
        Duration::from_secs(5),
    )
    .await
}

pub(crate) async fn runtime_fixture_with_intervals_and_account_refresh(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    recovery_retry_interval: Duration,
    audit_interval: Duration,
    account_refresh_interval: Duration,
) -> RuntimeFixture {
    runtime_fixture_with_options(
        restored_snapshot,
        position,
        open_orders,
        budget,
        RuntimeFixtureOptions {
            recovery_retry_interval,
            audit_interval,
            account_refresh_interval,
            clock: Arc::new(FixedClock(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            )),
        },
    )
    .await
}

pub(crate) async fn runtime_fixture_with_options(
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    position: Position,
    open_orders: Vec<ExchangeOrder>,
    budget: CapacityBudget,
    options: RuntimeFixtureOptions,
) -> RuntimeFixture {
    let (user_sender, user_receiver) = mpsc::channel(8);
    let exchange = Arc::new(FakeExchange::with_user_receiver(
        position,
        open_orders,
        user_receiver,
    ));
    let execution = exchange.execution_port();
    let account_summary = exchange.account_summary_port();
    let account = exchange.account_port();
    let metadata = exchange.metadata_port();
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));

    let mut manager = TrackManager::new(options.clock);
    manager
        .add_track(
            TrackId::new("BTCUSDT"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            test_config(),
            budget,
            exchange.exchange_info.rules.clone(),
        )
        .unwrap();

    if let Some(snapshot) = restored_snapshot.clone() {
        manager.restore_track_state(&snapshot).unwrap();
        persistence
            .save_transition("BTCUSDT", &snapshot, &[], &[])
            .await
            .unwrap();
    }

    let (events, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let account_monitor = build_test_account_monitor(account_summary, events.clone()).await;
    let services = build_test_application_services(
        manager,
        persistence.clone(),
        persistence.clone(),
        events.clone(),
        account_margin_guard.clone(),
    );
    let (state, worker_state) = build_runtime_and_effect_worker_test_contexts(
        &services,
        persistence.clone(),
        persistence.clone(),
        account_monitor,
        Arc::new(TrackProjector::new()),
    );

    RuntimeFixture {
        runtime: ServerRuntime::with_reconcile_and_account_refresh_intervals(
            state.runtime_state(),
            worker_state.effect_worker_state.clone(),
            RuntimePorts::new(
                execution,
                market_data as Arc<dyn MarketDataPort>,
                account,
                metadata,
            ),
            options.recovery_retry_interval,
            options.audit_interval,
            options.account_refresh_interval,
        ),
        state,
        worker: worker_state,
        exchange,
        persistence,
        price_sender,
        user_sender,
    }
}

pub(crate) async fn test_state<R>(
    metadata: Arc<dyn MetadataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    persistence: Arc<R>,
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    budget: CapacityBudget,
) -> RuntimeTestContext
where
    R: TrackMutationStore + TrackEffectStore + TrackQueryStore + 'static,
{
    test_launch_contexts(
        metadata,
        account_summary,
        persistence,
        restored_snapshot,
        budget,
    )
    .await
    .0
}

pub(crate) async fn test_launch_contexts<R>(
    metadata: Arc<dyn MetadataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    persistence: Arc<R>,
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    budget: CapacityBudget,
) -> (RuntimeTestContext, EffectWorkerTestContext)
where
    R: TrackMutationStore + TrackEffectStore + TrackQueryStore + 'static,
{
    test_launch_contexts_with_config(
        metadata,
        account_summary,
        persistence,
        restored_snapshot,
        budget,
        test_config(),
    )
    .await
}

pub(crate) async fn test_launch_contexts_with_config<R>(
    metadata: Arc<dyn MetadataPort>,
    account_summary: Arc<dyn AccountSummaryPort>,
    persistence: Arc<R>,
    restored_snapshot: Option<TrackRuntimeSnapshot>,
    budget: CapacityBudget,
    config: TrackConfig,
) -> (RuntimeTestContext, EffectWorkerTestContext)
where
    R: TrackMutationStore + TrackEffectStore + TrackQueryStore + 'static,
{
    let clock = Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
    ));
    let mut manager = TrackManager::new(clock);
    let instrument = btc_instrument();
    manager
        .add_track(
            TrackId::new("BTCUSDT"),
            instrument.clone(),
            config,
            budget,
            metadata.get_exchange_info(&instrument).await.unwrap().rules,
        )
        .unwrap();
    if let Some(snapshot) = restored_snapshot {
        manager.restore_track_state(&snapshot).unwrap();
    }

    let (events, _) = broadcast::channel(16);
    let mutation_store: Arc<dyn TrackMutationStore> = persistence.clone();
    let effect_store: Arc<dyn TrackEffectStore> = persistence.clone();
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let services = build_test_application_services(
        manager,
        mutation_store.clone(),
        effect_store.clone(),
        events.clone(),
        account_margin_guard.clone(),
    );
    let account_monitor = build_test_account_monitor(account_summary, events).await;
    let _ = Arc::new(TrackQueryService::new(
        persistence as Arc<dyn TrackQueryStore>,
        crate::test_support::test_prepared_registry("BTCUSDT"),
    ));
    build_runtime_and_effect_worker_test_contexts(
        &services,
        mutation_store,
        effect_store,
        account_monitor,
        Arc::new(TrackProjector::new()),
    )
}

pub(crate) async fn current_instance(
    state: &RuntimeTestContext,
) -> poise_engine::snapshot::TrackRuntimeSnapshot {
    let manager_handle = state.manager();
    let manager = manager_handle.read().await;
    manager.get_track("BTCUSDT").unwrap().snapshot()
}

pub(crate) async fn shutdown(handles: RuntimeHandles) {
    handles.market_task.abort();
    handles.user_task.abort();
    handles.effect_task.abort();
    handles.recovery_task.abort();
    handles.submit_preflight_task.abort();
    handles.account_task.abort();
    let _ = handles.market_task.await;
    let _ = handles.user_task.await;
    let _ = handles.effect_task.await;
    let _ = handles.recovery_task.await;
    let _ = handles.submit_preflight_task.await;
    let _ = handles.account_task.await;
}

pub(crate) async fn build_test_account_monitor(
    exchange: Arc<dyn AccountSummaryPort>,
    notifications: broadcast::Sender<poise_application::ApplicationNotification>,
) -> Arc<AccountMonitor> {
    let account_store: Arc<dyn AccountMonitorStore> =
        Arc::new(poise_storage::sqlite::SqliteStorage::in_memory().unwrap());
    account_store
        .save_state(&StoredAccountMonitorState {
            trading_day: chrono::NaiveDate::from_ymd_opt(2026, 3, 24).unwrap(),
            baseline_equity: 1_000_000.0,
            baseline_captured_at: Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
            last_observed_account_snapshot: None,
        })
        .await
        .unwrap();

    Arc::new(
        AccountMonitor::restore(
            exchange,
            account_store,
            notifications,
            AccountMonitorConfig::default(),
        )
        .await
        .unwrap(),
    )
}

pub(crate) async fn wait_until<F>(condition: F)
where
    F: Fn() -> bool,
{
    timeout(Duration::from_secs(1), async {
        loop {
            if condition() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) async fn wait_until_instance<F>(state: &RuntimeTestContext, predicate: F)
where
    F: Fn(&poise_engine::snapshot::TrackRuntimeSnapshot) -> bool,
{
    timeout(Duration::from_secs(1), async {
        loop {
            if predicate(&current_instance(state).await) {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) async fn wait_until_async<F, Fut>(condition: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    timeout(Duration::from_secs(1), async {
        loop {
            if condition().await {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

pub(crate) fn ready_pending_effects(effects: &[PersistedTrackEffect]) -> Vec<PersistedTrackEffect> {
    effects
        .iter()
        .filter(|effect| {
            effect.status == EffectStatus::Pending
                && !effects.iter().any(|prior| {
                    prior.track_id == effect.track_id
                        && prior.batch_id == effect.batch_id
                        && prior.sequence < effect.sequence
                        && !prior.status.unblocks_follow_up()
                })
        })
        .cloned()
        .collect()
}

pub(crate) fn apply_effect_status_update(
    effects: &mut [PersistedTrackEffect],
    effect_status_update: Option<&EffectStatusUpdate>,
    now: chrono::DateTime<Utc>,
) -> Result<()> {
    let Some(effect_status_update) = effect_status_update else {
        return Ok(());
    };
    let effect = effects
        .iter_mut()
        .find(|effect| effect.effect_id == effect_status_update.effect_id)
        .ok_or_else(|| anyhow!("effect `{}` not found", effect_status_update.effect_id))?;
    effect.status = effect_status_update.status;
    effect.attempt_count += effect_status_update.attempt_delta;
    effect.last_error = effect_status_update.last_error.clone();
    effect.updated_at = now;
    Ok(())
}

pub(crate) fn test_config() -> TrackConfig {
    TrackConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        long_exposure_units: 8.0,
        short_exposure_units: 8.0,
        notional_per_unit: 375.0,
        min_rebalance_units: 0.5,
        shape_family: ShapeFamily::Linear,
        out_of_band_policy: OutOfBandPolicy::Freeze,
    }
}

pub(crate) fn rounded_submit_test_config() -> TrackConfig {
    TrackConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        long_exposure_units: 6.0,
        short_exposure_units: 6.0,
        notional_per_unit: 333.0,
        min_rebalance_units: 0.5,
        shape_family: ShapeFamily::Linear,
        out_of_band_policy: OutOfBandPolicy::Freeze,
    }
}

pub(crate) fn test_budget() -> CapacityBudget {
    CapacityBudget {
        max_notional: 3000.0,
        daily_loss_limit: 120.0,
        total_loss_limit: 300.0,
    }
}

pub(crate) fn test_server_time() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()
}

pub(crate) fn btc_instrument() -> Instrument {
    Instrument::new(Venue::Binance, "BTCUSDT")
}

pub(crate) fn btc_position(qty: f64, unrealized_pnl: f64) -> Position {
    Position {
        instrument: btc_instrument(),
        qty,
        avg_price: 100.0,
        unrealized_pnl,
    }
}

pub(crate) fn btc_tick(reference_price: f64) -> PriceTick {
    PriceTick {
        instrument: btc_instrument(),
        mark_price: reference_price,
        execution_quote: Some(poise_engine::ports::ExecutionQuote {
            best_bid: reference_price,
            best_ask: reference_price,
        }),
        timestamp: Utc::now(),
    }
}

pub(crate) fn btc_exchange_order(
    order_id: &str,
    client_order_id: &str,
    side: Side,
    price: f64,
    qty: f64,
    realized_pnl: f64,
    status: OrderStatus,
) -> ExchangeOrder {
    ExchangeOrder {
        instrument: btc_instrument(),
        order_id: order_id.into(),
        client_order_id: client_order_id.into(),
        side,
        price,
        qty,
        realized_pnl,
        status,
    }
}

pub(crate) fn position_event_at(
    event_time: chrono::DateTime<Utc>,
    qty: f64,
    unrealized_pnl: f64,
) -> UserDataEvent {
    UserDataEvent {
        event_time,
        payload: UserDataPayload::PositionUpdate(btc_position(qty, unrealized_pnl)),
    }
}

pub(crate) fn order_event_at(
    event_time: chrono::DateTime<Utc>,
    order: ExchangeOrder,
) -> UserDataEvent {
    UserDataEvent {
        event_time,
        payload: UserDataPayload::OrderUpdate(order),
    }
}

pub(crate) fn test_snapshot() -> TrackRuntimeSnapshot {
    test_snapshot_with_config(test_config())
}

fn snapshot_restore_revision(
    config: &TrackConfig,
) -> poise_engine::persisted_runtime::TrackRestoreRevision {
    poise_engine::persisted_runtime::TrackRestoreRevision::for_track(&btc_instrument(), config)
}

pub(crate) fn working_order(
    order_id: Option<&str>,
    client_order_id: &str,
    side: Side,
    price: f64,
    quantity: f64,
    _desired_exposure: Exposure,
    status: OrderStatus,
) -> WorkingOrder {
    WorkingOrder {
        order_id: order_id.map(str::to_string),
        client_order_id: client_order_id.to_string(),
        side,
        price,
        quantity,
        status,
        role: match side {
            Side::Buy => OrderRole::IncreaseInventory,
            Side::Sell => OrderRole::DecreaseInventory,
        },
    }
}

pub(crate) fn set_executor_state(
    snapshot: &mut TrackRuntimeSnapshot,
    order: WorkingOrder,
    state: SlotState,
) {
    let desired_exposure = snapshot
        .desired_exposure
        .clone()
        .unwrap_or_else(|| snapshot.current_exposure.clone());
    snapshot.executor_state = ExecutorState {
        active_round: Some(poise_engine::runtime::ExecutionRound {
            desired_exposure: desired_exposure.clone(),
            mode: ExecutionMode::Passive,
            started_at: test_server_time(),
        }),
        diagnostics: poise_engine::runtime::ExecutorDiagnostics {
            mode: ExecutionMode::Passive,
            inventory_gap: snapshot.current_exposure.delta(&desired_exposure),
            gap_started_at: Some(test_server_time()),
            last_reprice_at: None,
            last_execution_reason: None,
            recovery_anomaly: None,
        },
        slots: vec![ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state,
            working_order: Some(order),
        }],
        recent_terminal_orders: Vec::new(),
        stats: ExecutionStats {
            started_at: test_server_time(),
            max_inventory_gap_abs: Exposure(0.0),
            max_gap_age_ms: 0,
        },
    };
}

pub(crate) fn inventory_core_order(
    track: &poise_engine::snapshot::TrackRuntimeSnapshot,
) -> Option<&WorkingOrder> {
    track
        .executor_state
        .slots
        .first()
        .and_then(|slot| slot.working_order.as_ref())
}

pub(crate) fn test_snapshot_with_config(config: TrackConfig) -> TrackRuntimeSnapshot {
    let mut snapshot = TrackRuntimeSnapshot {
        track_id: TrackId::new("BTCUSDT"),
        restore_revision: snapshot_restore_revision(&config),
        status: TrackStatus::Active,
        current_exposure: Exposure(0.0),
        desired_exposure: Some(Exposure(6.0)),
        manual_target_override: None,
        executor_state: ExecutorState::empty(test_server_time()),
        replacement_gate_reason: None,
        price_execution_block_reason: None,
        ledger_state: Default::default(),
        risk: RiskState::default(),
        observed: poise_engine::snapshot::ObservedState {
            strategy_price: Some(95.0),
            strategy_price_status: poise_engine::runtime::StrategyPriceStatus::Live,
            mark_price: Some(95.0),
            best_bid: Some(95.0),
            best_ask: Some(95.0),
            out_of_band_since: Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap()),
            last_tick_at: None,
            market_data_stale_since: None,
        },
    };
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("snapshot-1"),
            "snapshot-1",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    snapshot
}

pub(crate) struct FixedClock(pub(crate) chrono::DateTime<Utc>);

impl ClockPort for FixedClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        self.0
    }
}

#[derive(Clone)]
pub(crate) struct MutableClock(pub(crate) Arc<Mutex<chrono::DateTime<Utc>>>);

impl ClockPort for MutableClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

impl MutableClock {
    pub(crate) fn set(&self, value: chrono::DateTime<Utc>) {
        *self.0.lock().unwrap() = value;
    }
}

pub(crate) struct FakeExchange {
    pub(crate) exchange_info: ExchangeInfo,
    position: Mutex<Position>,
    pub(crate) open_orders: Mutex<Vec<ExchangeOrder>>,
    pub(crate) submitted_orders: Mutex<Vec<OrderRequest>>,
    pub(crate) canceled_order_ids: Mutex<Vec<String>>,
    pub(crate) cancel_all_symbols: Mutex<Vec<String>>,
    pub(crate) get_server_time_calls: AtomicUsize,
    pub(crate) get_position_calls: AtomicUsize,
    pub(crate) get_open_orders_calls: AtomicUsize,
    pub(crate) get_account_summary_calls: AtomicUsize,
    server_time_failures_remaining: AtomicUsize,
    position_failures_remaining: AtomicUsize,
    open_orders_failures_remaining: AtomicUsize,
    submit_error: Mutex<Option<String>>,
    cancel_order_error: Mutex<Option<CancelOrderFailure>>,
    cancel_all_error: Mutex<Option<String>>,
    server_time: chrono::DateTime<Utc>,
    sequence: AtomicUsize,
    submit_started: Option<Arc<Notify>>,
    release_submit: Option<Arc<Notify>>,
    get_position_started: Option<Arc<Notify>>,
    release_get_position: Mutex<Option<Arc<Notify>>>,
    user_receiver: AsyncMutex<Option<mpsc::Receiver<UserDataEvent>>>,
    subscribe_user_data_calls: AtomicUsize,
}

#[derive(Clone)]
enum CancelOrderFailure {
    Generic(String),
    OutcomeUnknown(String),
}

impl FakeExchange {
    pub(crate) fn new(position: Position, open_orders: Vec<ExchangeOrder>) -> Self {
        let (_sender, receiver) = mpsc::channel(1);
        Self::with_user_receiver(position, open_orders, receiver)
    }

    pub(crate) fn with_user_receiver(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        user_receiver: mpsc::Receiver<UserDataEvent>,
    ) -> Self {
        Self {
            exchange_info: ExchangeInfo {
                instrument: btc_instrument(),
                rules: ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            },
            position: Mutex::new(position),
            open_orders: Mutex::new(open_orders),
            submitted_orders: Mutex::new(Vec::new()),
            canceled_order_ids: Mutex::new(Vec::new()),
            cancel_all_symbols: Mutex::new(Vec::new()),
            get_server_time_calls: AtomicUsize::new(0),
            get_position_calls: AtomicUsize::new(0),
            get_open_orders_calls: AtomicUsize::new(0),
            get_account_summary_calls: AtomicUsize::new(0),
            server_time_failures_remaining: AtomicUsize::new(0),
            position_failures_remaining: AtomicUsize::new(0),
            open_orders_failures_remaining: AtomicUsize::new(0),
            submit_error: Mutex::new(None),
            cancel_order_error: Mutex::new(None),
            cancel_all_error: Mutex::new(None),
            server_time: test_server_time(),
            sequence: AtomicUsize::new(1),
            submit_started: None,
            release_submit: None,
            get_position_started: None,
            release_get_position: Mutex::new(None),
            user_receiver: AsyncMutex::new(Some(user_receiver)),
            subscribe_user_data_calls: AtomicUsize::new(0),
        }
    }

    pub(crate) fn with_submit_error(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        error: &str,
    ) -> Self {
        let exchange = Self::new(position, open_orders);
        *exchange.submit_error.lock().unwrap() = Some(error.to_string());
        exchange
    }

    pub(crate) fn with_cancel_order_error(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        error: &str,
    ) -> Self {
        let exchange = Self::new(position, open_orders);
        *exchange.cancel_order_error.lock().unwrap() =
            Some(CancelOrderFailure::Generic(error.to_string()));
        exchange
    }

    pub(crate) fn with_cancel_order_outcome_unknown(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        error: &str,
    ) -> Self {
        let exchange = Self::new(position, open_orders);
        *exchange.cancel_order_error.lock().unwrap() =
            Some(CancelOrderFailure::OutcomeUnknown(error.to_string()));
        exchange
    }

    pub(crate) fn with_blocked_submit(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        submit_started: Arc<Notify>,
        release_submit: Arc<Notify>,
    ) -> Self {
        let mut exchange = Self::new(position, open_orders);
        exchange.submit_started = Some(submit_started);
        exchange.release_submit = Some(release_submit);
        exchange
    }

    pub(crate) fn with_blocked_get_position(
        position: Position,
        open_orders: Vec<ExchangeOrder>,
        get_position_started: Arc<Notify>,
        release_get_position: Arc<Notify>,
    ) -> Self {
        let mut exchange = Self::new(position, open_orders);
        exchange.get_position_started = Some(get_position_started);
        *exchange.release_get_position.lock().unwrap() = Some(release_get_position);
        exchange
    }

    pub(crate) fn fail_next_server_time_requests(&self, count: usize) {
        self.server_time_failures_remaining
            .store(count, Ordering::SeqCst);
    }

    pub(crate) fn fail_next_open_orders_requests(&self, count: usize) {
        self.open_orders_failures_remaining
            .store(count, Ordering::SeqCst);
    }

    pub(crate) fn set_open_orders(&self, open_orders: Vec<ExchangeOrder>) {
        *self.open_orders.lock().unwrap() = open_orders;
    }

    pub(crate) fn set_position(&self, position: Position) {
        *self.position.lock().unwrap() = position;
    }

    pub(crate) fn account_summary_port(self: &Arc<Self>) -> Arc<dyn AccountSummaryPort> {
        Arc::new(FakeExchangeAccountSummaryPort {
            exchange: Arc::clone(self),
        })
    }

    pub(crate) fn execution_port(self: &Arc<Self>) -> Arc<dyn ExecutionPort> {
        Arc::new(FakeExchangeExecutionPort {
            exchange: Arc::clone(self),
        })
    }

    pub(crate) fn account_port(self: &Arc<Self>) -> Arc<dyn AccountPort> {
        Arc::new(FakeExchangeAccountPort {
            exchange: Arc::clone(self),
        })
    }

    pub(crate) fn metadata_port(self: &Arc<Self>) -> Arc<dyn MetadataPort> {
        Arc::new(FakeExchangeMetadataPort {
            exchange: Arc::clone(self),
        })
    }
}

#[async_trait::async_trait]
impl poise_engine::ports::AccountSummaryPort for FakeExchangeAccountSummaryPort {
    async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
        self.exchange
            .get_account_summary_calls
            .fetch_add(1, Ordering::SeqCst);
        Ok(poise_engine::ports::AccountSummarySnapshot {
            equity: 1_000_000.0,
            available: 1_000_000.0,
            unrealized_pnl: 0.0,
            observed_at: Utc::now(),
        })
    }
}

#[async_trait::async_trait]
impl ExecutionPort for FakeExchangeExecutionPort {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.exchange
            .submitted_orders
            .lock()
            .unwrap()
            .push(req.clone());
        if let Some(notify) = &self.exchange.submit_started {
            notify.notify_waiters();
        }
        if let Some(notify) = &self.exchange.release_submit {
            notify.notified().await;
        }
        if let Some(error) = self.exchange.submit_error.lock().unwrap().clone() {
            return Err(anyhow!(error));
        }
        let order_id = self.exchange.sequence.fetch_add(1, Ordering::SeqCst);
        Ok(OrderReceipt {
            order_id: format!("order-{order_id}"),
            client_order_id: req.client_order_id,
            status: OrderStatus::New,
        })
    }

    async fn cancel_order(&self, _instrument: &Instrument, order_id: &str) -> Result<()> {
        self.exchange
            .canceled_order_ids
            .lock()
            .unwrap()
            .push(order_id.to_string());
        if let Some(error) = self.exchange.cancel_order_error.lock().unwrap().clone() {
            return Err(match error {
                CancelOrderFailure::Generic(message) => anyhow!(message),
                CancelOrderFailure::OutcomeUnknown(message) => {
                    poise_engine::ports::ExecutionPortError::cancel_outcome_unknown(message).into()
                }
            });
        }
        self.exchange
            .open_orders
            .lock()
            .unwrap()
            .retain(|order| order.order_id != order_id);
        Ok(())
    }

    async fn cancel_all(&self, instrument: &Instrument) -> Result<()> {
        self.exchange
            .cancel_all_symbols
            .lock()
            .unwrap()
            .push(instrument.symbol.clone());
        if let Some(error) = self.exchange.cancel_all_error.lock().unwrap().clone() {
            return Err(anyhow!(error));
        }
        self.exchange.open_orders.lock().unwrap().clear();
        Ok(())
    }

    async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
        self.exchange
            .get_position_calls
            .fetch_add(1, Ordering::SeqCst);
        if let Some(notify) = &self.exchange.get_position_started {
            notify.notify_waiters();
        }
        let release_notify = { self.exchange.release_get_position.lock().unwrap().take() };
        if let Some(notify) = release_notify {
            notify.notified().await;
        }
        if self
            .exchange
            .position_failures_remaining
            .load(Ordering::SeqCst)
            > 0
        {
            self.exchange
                .position_failures_remaining
                .fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("temporary get_position timeout"));
        }
        Ok(self.exchange.position.lock().unwrap().clone())
    }

    async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        self.exchange
            .get_open_orders_calls
            .fetch_add(1, Ordering::SeqCst);
        if self
            .exchange
            .open_orders_failures_remaining
            .load(Ordering::SeqCst)
            > 0
        {
            self.exchange
                .open_orders_failures_remaining
                .fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("temporary get_open_orders timeout"));
        }
        Ok(self.exchange.open_orders.lock().unwrap().clone())
    }
}

#[async_trait::async_trait]
impl AccountPort for FakeExchangeAccountPort {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
        Ok(poise_engine::ports::AccountCapacitySnapshot {
            max_increase_notional: 1_000_000.0,
        })
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.exchange
            .subscribe_user_data_calls
            .fetch_add(1, Ordering::SeqCst);
        self.exchange
            .user_receiver
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow!("missing test user receiver"))
    }
}

#[async_trait::async_trait]
impl MetadataPort for FakeExchangeMetadataPort {
    async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
        Ok(self.exchange.exchange_info.clone())
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        self.exchange
            .get_server_time_calls
            .fetch_add(1, Ordering::SeqCst);
        if self
            .exchange
            .server_time_failures_remaining
            .load(Ordering::SeqCst)
            > 0
        {
            self.exchange
                .server_time_failures_remaining
                .fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("temporary get_server_time timeout"));
        }
        Ok(self.exchange.server_time)
    }
}

struct FakeExchangeAccountSummaryPort {
    exchange: Arc<FakeExchange>,
}

struct FakeExchangeExecutionPort {
    exchange: Arc<FakeExchange>,
}

struct FakeExchangeAccountPort {
    exchange: Arc<FakeExchange>,
}

struct FakeExchangeMetadataPort {
    exchange: Arc<FakeExchange>,
}

#[derive(Default)]
pub(crate) struct MemoryPersistence {
    snapshots: AsyncMutex<HashMap<String, TrackRuntimeSnapshot>>,
    effects: AsyncMutex<Vec<PersistedTrackEffect>>,
    follow_up_retirements: AsyncMutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
    next_effect_batch: AtomicUsize,
    pub(crate) save_transition_count: AtomicUsize,
    fail_next_load_track_state_requests: AtomicUsize,
    fail_next_pending_submit_effect_queries: AtomicUsize,
}

#[async_trait::async_trait]
impl TrackMutationStore for MemoryPersistence {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        _events: &[poise_core::events::DomainEvent],
        effects: &[ExecutionAction],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        self.save_transition_count.fetch_add(1, Ordering::SeqCst);
        self.snapshots
            .lock()
            .await
            .insert(id.to_string(), state.clone());

        let now = Utc::now();
        let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let mut effect_store = self.effects.lock().await;
        let mut persisted_effects = Vec::new();
        for (sequence, effect) in effects.iter().enumerate() {
            if matches!(effect, ExecutionAction::NoOp) {
                continue;
            }

            let persisted = PersistedTrackEffect {
                effect_id: format!("{id}:{batch_id}:{sequence}"),
                track_id: TrackId::new(id),
                batch_id: batch_id.clone(),
                sequence: u32::try_from(sequence).unwrap(),
                effect: effect.clone(),
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: now,
                updated_at: now,
            };
            effect_store.push(persisted.clone());
            persisted_effects.push(persisted);
        }
        apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

        Ok(CommittedTrackWrite {
            track_id: TrackId::new(id),
            effects: persisted_effects,
        })
    }

    async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
        if self
            .fail_next_load_track_state_requests
            .load(Ordering::SeqCst)
            > 0
        {
            self.fail_next_load_track_state_requests
                .fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("injected load_track_state failure"));
        }
        Ok(self.snapshots.lock().await.get(id).cloned())
    }

    async fn list_track_events(&self, _id: &str) -> Result<Vec<poise_core::events::DomainEvent>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl TrackEffectStore for MemoryPersistence {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects))
    }

    async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        if self
            .fail_next_pending_submit_effect_queries
            .load(Ordering::SeqCst)
            > 0
        {
            self.fail_next_pending_submit_effect_queries
                .fetch_sub(1, Ordering::SeqCst);
            return Err(anyhow!("injected pending submit effect query failure"));
        }
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn list_pending_submit_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects)
            .into_iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(effects
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| effect.batch_id == batch_id)
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn save_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        let entry = stored.entry(track_id.clone()).or_default();
        if !entry.contains(request) {
            entry.push(request.clone());
        }
        Ok(())
    }

    async fn list_follow_up_retirement_requests(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<FollowUpRetirementRequest>> {
        Ok(self
            .follow_up_retirements
            .lock()
            .await
            .get(track_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn delete_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        if let Some(existing) = stored.get_mut(track_id) {
            existing.retain(|candidate| candidate != request);
            if existing.is_empty() {
                stored.remove(track_id);
            }
        }
        Ok(())
    }
}

impl MemoryPersistence {
    pub(crate) fn save_transition_count(&self) -> usize {
        self.save_transition_count.load(Ordering::SeqCst)
    }

    pub(crate) fn fail_next_pending_submit_effect_queries(&self, count: usize) {
        self.fail_next_pending_submit_effect_queries
            .store(count, Ordering::SeqCst);
    }

    pub(crate) async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
        self.effects.lock().await.clone()
    }

    pub(crate) async fn seed_effect(&self, effect: PersistedTrackEffect) {
        self.effects.lock().await.push(effect);
    }
}

#[async_trait::async_trait]
impl TrackQueryStore for MemoryPersistence {
    async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .values()
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            })
            .collect())
    }

    async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .get(track_id.as_str())
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            }))
    }

    async fn list_recent_track_events(
        &self,
        _track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<StoredTrackEvent>> {
        Ok(Vec::new())
    }

    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .cloned()
            .collect())
    }
}

#[derive(Default)]
pub(crate) struct FailOnReceiptPersistence {
    snapshots: AsyncMutex<HashMap<String, TrackRuntimeSnapshot>>,
    effects: AsyncMutex<Vec<PersistedTrackEffect>>,
    follow_up_retirements: AsyncMutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
    next_effect_batch: AtomicUsize,
}

#[async_trait::async_trait]
impl TrackMutationStore for FailOnReceiptPersistence {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        _events: &[poise_core::events::DomainEvent],
        effects: &[ExecutionAction],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        if state
            .executor_state
            .slots
            .first()
            .and_then(|slot| slot.working_order.as_ref())
            .and_then(|order| order.order_id.as_ref())
            .is_some()
        {
            return Err(anyhow!("injected receipt persistence failure"));
        }

        self.snapshots
            .lock()
            .await
            .insert(id.to_string(), state.clone());

        let now = Utc::now();
        let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let mut effect_store = self.effects.lock().await;
        let mut persisted_effects = Vec::new();
        for (sequence, effect) in effects.iter().enumerate() {
            if matches!(effect, ExecutionAction::NoOp) {
                continue;
            }

            let persisted = PersistedTrackEffect {
                effect_id: format!("{id}:{batch_id}:{sequence}"),
                track_id: TrackId::new(id),
                batch_id: batch_id.clone(),
                sequence: u32::try_from(sequence).unwrap(),
                effect: effect.clone(),
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: now,
                updated_at: now,
            };
            effect_store.push(persisted.clone());
            persisted_effects.push(persisted);
        }
        apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

        Ok(CommittedTrackWrite {
            track_id: TrackId::new(id),
            effects: persisted_effects,
        })
    }

    async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
        Ok(self.snapshots.lock().await.get(id).cloned())
    }

    async fn list_track_events(&self, _id: &str) -> Result<Vec<poise_core::events::DomainEvent>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl TrackEffectStore for FailOnReceiptPersistence {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects))
    }

    async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn list_pending_submit_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects)
            .into_iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(effects
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| effect.batch_id == batch_id)
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn save_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        let entry = stored.entry(track_id.clone()).or_default();
        if !entry.contains(request) {
            entry.push(request.clone());
        }
        Ok(())
    }

    async fn list_follow_up_retirement_requests(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<FollowUpRetirementRequest>> {
        Ok(self
            .follow_up_retirements
            .lock()
            .await
            .get(track_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn delete_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        if let Some(existing) = stored.get_mut(track_id) {
            existing.retain(|candidate| candidate != request);
            if existing.is_empty() {
                stored.remove(track_id);
            }
        }
        Ok(())
    }
}

impl FailOnReceiptPersistence {
    pub(crate) async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
        self.effects.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl TrackQueryStore for FailOnReceiptPersistence {
    async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .values()
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            })
            .collect())
    }

    async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .get(track_id.as_str())
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            }))
    }

    async fn list_recent_track_events(
        &self,
        _track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<StoredTrackEvent>> {
        Ok(Vec::new())
    }

    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .cloned()
            .collect())
    }
}

pub(crate) struct FailOnSavePersistence {
    snapshots: AsyncMutex<HashMap<String, TrackRuntimeSnapshot>>,
    effects: AsyncMutex<Vec<PersistedTrackEffect>>,
    follow_up_retirements: AsyncMutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
    next_effect_batch: AtomicUsize,
    save_count: AtomicUsize,
    fail_on: usize,
}

impl FailOnSavePersistence {
    pub(crate) fn new(fail_on: usize) -> Self {
        Self {
            snapshots: AsyncMutex::new(HashMap::new()),
            effects: AsyncMutex::new(Vec::new()),
            follow_up_retirements: AsyncMutex::new(HashMap::new()),
            next_effect_batch: AtomicUsize::new(0),
            save_count: AtomicUsize::new(0),
            fail_on,
        }
    }

    pub(crate) async fn seed_snapshot(&self, id: &str, snapshot: TrackRuntimeSnapshot) {
        self.snapshots.lock().await.insert(id.to_string(), snapshot);
    }

    pub(crate) async fn seed_effect(&self, effect: PersistedTrackEffect) {
        self.effects.lock().await.push(effect);
    }

    pub(crate) async fn all_effects(&self) -> Vec<PersistedTrackEffect> {
        self.effects.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl TrackMutationStore for FailOnSavePersistence {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &TrackRuntimeSnapshot,
        _events: &[poise_core::events::DomainEvent],
        effects: &[ExecutionAction],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        let save_number = self.save_count.fetch_add(1, Ordering::SeqCst) + 1;
        if save_number == self.fail_on {
            return Err(anyhow!("injected save failure"));
        }

        self.snapshots
            .lock()
            .await
            .insert(id.to_string(), state.clone());

        let now = Utc::now();
        let batch_id = (self.next_effect_batch.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let mut effect_store = self.effects.lock().await;
        let mut persisted_effects = Vec::new();
        for (sequence, effect) in effects.iter().enumerate() {
            if matches!(effect, ExecutionAction::NoOp) {
                continue;
            }

            let persisted = PersistedTrackEffect {
                effect_id: format!("{id}:{batch_id}:{sequence}"),
                track_id: TrackId::new(id),
                batch_id: batch_id.clone(),
                sequence: u32::try_from(sequence).unwrap(),
                effect: effect.clone(),
                status: EffectStatus::Pending,
                attempt_count: 0,
                last_error: None,
                created_at: now,
                updated_at: now,
            };
            effect_store.push(persisted.clone());
            persisted_effects.push(persisted);
        }
        apply_effect_status_update(&mut effect_store, effect_status_update, now)?;

        Ok(CommittedTrackWrite {
            track_id: TrackId::new(id),
            effects: persisted_effects,
        })
    }

    async fn load_track_state(&self, id: &str) -> Result<Option<TrackRuntimeSnapshot>> {
        Ok(self.snapshots.lock().await.get(id).cloned())
    }

    async fn list_track_events(&self, _id: &str) -> Result<Vec<poise_core::events::DomainEvent>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl TrackEffectStore for FailOnSavePersistence {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects))
    }

    async fn list_all_pending_submit_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn list_pending_submit_effects_for_track(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(ready_pending_effects(&effects)
            .into_iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .collect())
    }

    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>> {
        let effects = self.effects.lock().await;
        Ok(effects
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| effect.batch_id == batch_id)
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn save_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        let entry = stored.entry(track_id.clone()).or_default();
        if !entry.contains(request) {
            entry.push(request.clone());
        }
        Ok(())
    }

    async fn list_follow_up_retirement_requests(
        &self,
        track_id: &TrackId,
    ) -> Result<Vec<FollowUpRetirementRequest>> {
        Ok(self
            .follow_up_retirements
            .lock()
            .await
            .get(track_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn delete_follow_up_retirement_request(
        &self,
        track_id: &TrackId,
        request: &FollowUpRetirementRequest,
    ) -> Result<()> {
        let mut stored = self.follow_up_retirements.lock().await;
        if let Some(existing) = stored.get_mut(track_id) {
            existing.retain(|candidate| candidate != request);
            if existing.is_empty() {
                stored.remove(track_id);
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl TrackQueryStore for FailOnSavePersistence {
    async fn list_track_snapshots(&self) -> Result<Vec<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .values()
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            })
            .collect())
    }

    async fn load_track_snapshot(&self, track_id: &TrackId) -> Result<Option<StoredTrackSnapshot>> {
        Ok(self
            .snapshots
            .lock()
            .await
            .get(track_id.as_str())
            .cloned()
            .map(|snapshot| StoredTrackSnapshot {
                snapshot,
                updated_at: Utc::now(),
            }))
    }

    async fn list_recent_track_events(
        &self,
        _track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<StoredTrackEvent>> {
        Ok(Vec::new())
    }

    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        _limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .cloned()
            .collect())
    }
}

pub(crate) struct FakeMarketData {
    price_receivers: Mutex<HashMap<String, mpsc::Receiver<PriceTick>>>,
}

impl FakeMarketData {
    pub(crate) fn new(price_receiver: mpsc::Receiver<PriceTick>) -> Self {
        let mut price_receivers = HashMap::new();
        price_receivers.insert("BTCUSDT".to_string(), price_receiver);
        Self {
            price_receivers: Mutex::new(price_receivers),
        }
    }

    pub(crate) fn without_user_receiver(price_receiver: mpsc::Receiver<PriceTick>) -> Self {
        Self::new(price_receiver)
    }
}

#[async_trait::async_trait]
impl MarketDataPort for FakeMarketData {
    async fn subscribe_prices(&self, instrument: &Instrument) -> Result<mpsc::Receiver<PriceTick>> {
        self.price_receivers
            .lock()
            .unwrap()
            .remove(&instrument.symbol)
            .ok_or_else(|| anyhow!("missing test price receiver for {}", instrument.symbol))
    }
}

pub(crate) struct RuntimeWithPortsFixture {
    pub(crate) runtime: ServerRuntime,
    pub(crate) state: RuntimeTestContext,
    pub(crate) account: Arc<FakeAccountPort>,
}

pub(crate) async fn build_test_runtime_with_ports(
    execution: Arc<FakeExecutionPort>,
    market_data: Arc<FakeMarketDataPort>,
    account_summary: Arc<FakeAccountSummaryPort>,
    account: Arc<FakeAccountPort>,
    metadata: Arc<FakeMetadataPort>,
) -> RuntimeWithPortsFixture {
    let persistence = Arc::new(MemoryPersistence::default());
    let mut manager = TrackManager::new(Arc::new(FixedClock(test_server_time())));
    let instrument = btc_instrument();
    manager
        .add_track(
            TrackId::new("BTCUSDT"),
            instrument.clone(),
            test_config(),
            test_budget(),
            metadata.get_exchange_info(&instrument).await.unwrap().rules,
        )
        .unwrap();

    let (events, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let services = build_test_application_services(
        manager,
        persistence.clone(),
        persistence.clone(),
        events.clone(),
        account_margin_guard,
    );
    let account_monitor = Arc::new(
        AccountMonitor::restore(
            account_summary,
            Arc::new(poise_storage::sqlite::SqliteStorage::in_memory().unwrap()),
            events,
            AccountMonitorConfig::default(),
        )
        .await
        .unwrap(),
    );
    let (state, worker_state) = build_runtime_and_effect_worker_test_contexts(
        &services,
        persistence.clone(),
        persistence,
        account_monitor,
        Arc::new(TrackProjector::new()),
    );

    RuntimeWithPortsFixture {
        runtime: ServerRuntime::with_account_capacity_snapshots(
            state.runtime_state(),
            worker_state.effect_worker_state,
            RuntimePorts::new(execution, market_data, account.clone(), metadata),
            HashMap::new(),
            Duration::from_secs(1),
        ),
        state,
        account,
    }
}

pub(crate) struct FakeExecutionPort {
    position: Mutex<Position>,
}

impl FakeExecutionPort {
    pub(crate) fn default_position() -> Position {
        btc_position(0.0, 0.0)
    }
}

#[async_trait::async_trait]
impl ExecutionPort for FakeExecutionPort {
    async fn submit_order(&self, _req: OrderRequest) -> Result<OrderReceipt> {
        Err(anyhow!("not used in tests"))
    }

    async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
        Err(anyhow!("not used in tests"))
    }

    async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
        Err(anyhow!("not used in tests"))
    }

    async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
        Ok(self.position.lock().unwrap().clone())
    }

    async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        Ok(Vec::new())
    }
}

impl Default for FakeExecutionPort {
    fn default() -> Self {
        Self {
            position: Mutex::new(Self::default_position()),
        }
    }
}

#[derive(Default)]
pub(crate) struct FakeMarketDataPort;

#[async_trait::async_trait]
impl MarketDataPort for FakeMarketDataPort {
    async fn subscribe_prices(
        &self,
        _instrument: &Instrument,
    ) -> Result<mpsc::Receiver<PriceTick>> {
        let (_sender, receiver) = mpsc::channel(1);
        Ok(receiver)
    }
}

#[derive(Default)]
pub(crate) struct FakeAccountSummaryPort;

#[async_trait::async_trait]
impl AccountSummaryPort for FakeAccountSummaryPort {
    async fn get_account_summary(&self) -> Result<poise_engine::ports::AccountSummarySnapshot> {
        Ok(poise_engine::ports::AccountSummarySnapshot {
            equity: 1_000_000.0,
            available: 1_000_000.0,
            unrealized_pnl: 0.0,
            observed_at: test_server_time(),
        })
    }
}

pub(crate) struct FakeAccountPort {
    user_receiver: AsyncMutex<Option<mpsc::Receiver<UserDataEvent>>>,
    subscribe_user_data_calls: AtomicUsize,
}

impl FakeAccountPort {
    pub(crate) fn with_user_events(events: Vec<UserDataEvent>) -> Self {
        let (sender, receiver) = mpsc::channel(events.len().max(1));
        for event in events {
            sender.try_send(event).unwrap();
        }
        drop(sender);
        Self {
            user_receiver: AsyncMutex::new(Some(receiver)),
            subscribe_user_data_calls: AtomicUsize::new(0),
        }
    }

    pub(crate) fn without_user_receiver() -> Self {
        Self {
            user_receiver: AsyncMutex::new(None),
            subscribe_user_data_calls: AtomicUsize::new(0),
        }
    }

    pub(crate) fn subscribe_user_data_calls(&self) -> usize {
        self.subscribe_user_data_calls.load(Ordering::SeqCst)
    }
}

impl Default for FakeAccountPort {
    fn default() -> Self {
        Self::with_user_events(Vec::new())
    }
}

#[async_trait::async_trait]
impl AccountPort for FakeAccountPort {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<AccountCapacitySnapshot> {
        Ok(AccountCapacitySnapshot {
            max_increase_notional: 1_000_000.0,
        })
    }

    async fn subscribe_user_data(&self) -> Result<mpsc::Receiver<UserDataEvent>> {
        self.subscribe_user_data_calls
            .fetch_add(1, Ordering::SeqCst);
        self.user_receiver
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow!("missing test user receiver"))
    }
}

#[derive(Default)]
pub(crate) struct FakeMetadataPort;

#[async_trait::async_trait]
impl MetadataPort for FakeMetadataPort {
    async fn get_exchange_info(&self, _instrument: &Instrument) -> Result<ExchangeInfo> {
        Ok(ExchangeInfo {
            instrument: btc_instrument(),
            rules: ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
        })
    }

    async fn get_server_time(&self) -> Result<chrono::DateTime<Utc>> {
        Ok(test_server_time())
    }
}
