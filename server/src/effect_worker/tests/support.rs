use super::*;
use poise_engine::ports::{ExecutionPortError, UserDataEvent};

pub(crate) async fn test_state(repository: Arc<MemoryRepository>) -> EffectWorkerTestContext {
    test_state_with_track(
        repository,
        test_config(),
        ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        },
    )
    .await
}

pub(crate) async fn test_state_with_track(
    repository: Arc<MemoryRepository>,
    config: TrackConfig,
    exchange_rules: ExchangeRules,
) -> EffectWorkerTestContext {
    let clock = Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
    ));
    let mut manager = TrackManager::new(clock);
    let instrument = btc_instrument();
    manager
        .add_track(
            TrackId::new("btc-core"),
            instrument.clone(),
            config,
            test_budget(),
            exchange_rules,
        )
        .unwrap();

    let (notifications, _) = broadcast::channel(16);
    let mutation_store: Arc<dyn TrackMutationStore> = repository.clone();
    let effect_store: Arc<dyn TrackEffectStore> = repository.clone();
    let account_margin_guard = Arc::new(crate::runtime::AccountMarginGuardStore::default());
    let services = build_test_application_services(
        manager,
        mutation_store.clone(),
        effect_store.clone(),
        notifications.clone(),
        account_margin_guard.clone(),
    );
    build_effect_worker_test_context(&services, mutation_store, effect_store)
}

pub(crate) fn btc_instrument() -> Instrument {
    Instrument::new(Venue::Binance, "BTCUSDT")
}

fn snapshot_restore_revision(
    config: &TrackConfig,
) -> poise_engine::persisted_runtime::TrackRestoreRevision {
    poise_engine::persisted_runtime::TrackRestoreRevision::for_track(&btc_instrument(), config)
}

pub(crate) fn snapshot_with_recovery_anomaly() -> TrackRuntimeSnapshot {
    let config = test_config();
    TrackRuntimeSnapshot {
        track_id: TrackId::new("btc-core"),
        restore_revision: snapshot_restore_revision(&config),
        status: TrackStatus::Active,
        current_exposure: Exposure(0.0),
        desired_exposure: Some(Exposure(6.0)),
        manual_target_override: None,
        executor_state: ExecutorState {
            active_round: Some(poise_engine::runtime::ExecutionRound {
                desired_exposure: Exposure(6.0),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
            }),
            diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(6.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: Some(RecoveryAnomaly::UnknownLiveOrder),
            },
            slots: vec![poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                state: SlotState::Empty,
                working_order: None,
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: Exposure(6.0),
                max_gap_age_ms: 0,
            },
        },
        replacement_gate_reason: None,
        price_execution_block_reason: None,
        ledger_state: Default::default(),
        risk: RiskState::default(),
        observed: ObservedState {
            strategy_price: None,
            strategy_price_status: poise_engine::runtime::StrategyPriceStatus::Stale,
            mark_price: None,
            best_bid: None,
            best_ask: None,
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        },
    }
}

pub(crate) fn snapshot_with_working_order() -> TrackRuntimeSnapshot {
    let config = test_config();
    TrackRuntimeSnapshot {
        track_id: TrackId::new("btc-core"),
        restore_revision: snapshot_restore_revision(&config),
        status: TrackStatus::Active,
        current_exposure: Exposure(2.0),
        desired_exposure: Some(Exposure(6.0)),
        manual_target_override: None,
        executor_state: ExecutorState {
            active_round: Some(poise_engine::runtime::ExecutionRound {
                desired_exposure: Exposure(6.0),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
            }),
            diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(4.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(poise_engine::runtime::WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    status: OrderStatus::New,
                    role: poise_engine::executor::OrderRole::IncreaseInventory,
                }),
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: Exposure(4.0),
                max_gap_age_ms: 0,
            },
        },
        replacement_gate_reason: None,
        price_execution_block_reason: None,
        ledger_state: Default::default(),
        risk: RiskState::default(),
        observed: ObservedState {
            strategy_price: None,
            strategy_price_status: poise_engine::runtime::StrategyPriceStatus::Stale,
            mark_price: None,
            best_bid: None,
            best_ask: None,
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        },
    }
}

pub(crate) fn snapshot_with_submit_pending_order(
    reference_price: f64,
    config: TrackConfig,
    order: WorkingOrder,
) -> TrackRuntimeSnapshot {
    TrackRuntimeSnapshot {
        track_id: TrackId::new("btc-core"),
        restore_revision: snapshot_restore_revision(&config),
        status: TrackStatus::Active,
        current_exposure: Exposure(0.0),
        desired_exposure: Some(poise_core::strategy::desired_exposure(
            reference_price,
            &config,
        )),
        manual_target_override: None,
        executor_state: ExecutorState {
            active_round: Some(poise_engine::runtime::ExecutionRound {
                desired_exposure: poise_core::strategy::desired_exposure(reference_price, &config),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
            }),
            diagnostics: poise_engine::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: Exposure(
                    poise_core::strategy::desired_exposure(reference_price, &config).0,
                ),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: Some(order),
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 24, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: Exposure(0.0),
                max_gap_age_ms: 0,
            },
        },
        replacement_gate_reason: None,
        price_execution_block_reason: None,
        ledger_state: Default::default(),
        risk: RiskState::default(),
        observed: ObservedState {
            strategy_price: None,
            strategy_price_status: poise_engine::runtime::StrategyPriceStatus::Stale,
            mark_price: None,
            best_bid: None,
            best_ask: None,
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        },
    }
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
        out_of_band_policy: BandProtectionPolicy::Freeze {
            recover: BandRecoverPolicy::BackInBand,
        },
    }
}

pub(crate) fn test_budget() -> CapacityBudget {
    CapacityBudget {
        max_notional: 3000.0,
        daily_loss_limit: 120.0,
        total_loss_limit: 300.0,
    }
}

pub(crate) struct FixedClock(chrono::DateTime<Utc>);

impl ClockPort for FixedClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        self.0
    }
}

pub(crate) struct FakeExchange {
    pub(crate) effects: AsyncMutex<Vec<OrderRequest>>,
    submit_started: Option<Arc<Notify>>,
    release_submit: Option<Arc<Notify>>,
    get_position_started: Option<Arc<Notify>>,
    release_get_position: Option<Arc<Notify>>,
    cancel_order_error: Option<String>,
    position: AsyncMutex<Position>,
    pub(crate) open_orders: AsyncMutex<Vec<ExchangeOrder>>,
    get_position_calls: AtomicUsize,
    get_open_orders_calls: AtomicUsize,
}

impl FakeExchange {
    fn default_with_state() -> Self {
        Self {
            effects: AsyncMutex::default(),
            submit_started: None,
            release_submit: None,
            get_position_started: None,
            release_get_position: None,
            cancel_order_error: None,
            position: AsyncMutex::new(Position {
                instrument: btc_instrument(),
                qty: 0.0,
                avg_price: 100.0,
                unrealized_pnl: 0.0,
            }),
            open_orders: AsyncMutex::new(Vec::new()),
            get_position_calls: AtomicUsize::new(0),
            get_open_orders_calls: AtomicUsize::new(0),
        }
    }

    pub(crate) fn with_blocked_submit(
        submit_started: Arc<Notify>,
        release_submit: Arc<Notify>,
    ) -> Self {
        Self {
            submit_started: Some(submit_started),
            release_submit: Some(release_submit),
            ..Self::default()
        }
    }

    pub(crate) fn with_blocked_submit_and_get_position(
        submit_started: Arc<Notify>,
        release_submit: Arc<Notify>,
        get_position_started: Arc<Notify>,
        release_get_position: Arc<Notify>,
    ) -> Self {
        Self {
            submit_started: Some(submit_started),
            release_submit: Some(release_submit),
            get_position_started: Some(get_position_started),
            release_get_position: Some(release_get_position),
            ..Self::default()
        }
    }

    pub(crate) fn with_cancel_order_outcome_unknown(message: &str) -> Self {
        Self {
            cancel_order_error: Some(message.to_string()),
            ..Self::default()
        }
    }

    pub(crate) async fn set_position_qty(&self, qty: f64) {
        let mut position = self.position.lock().await;
        position.qty = qty;
    }

    pub(crate) fn get_position_calls(&self) -> usize {
        self.get_position_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn get_open_orders_calls(&self) -> usize {
        self.get_open_orders_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn execution_port(self: &Arc<Self>) -> Arc<dyn ExecutionPort> {
        Arc::new(FakeExecutionPort {
            exchange: Arc::clone(self),
        })
    }

    pub(crate) fn account_port(self: &Arc<Self>) -> Arc<dyn AccountPort> {
        Arc::new(FakeAccountPort {
            exchange: Arc::clone(self),
        })
    }
}

impl Default for FakeExchange {
    fn default() -> Self {
        Self::default_with_state()
    }
}

struct FakeExecutionPort {
    exchange: Arc<FakeExchange>,
}

#[async_trait::async_trait]
impl ExecutionPort for FakeExecutionPort {
    async fn submit_order(&self, req: OrderRequest) -> Result<OrderReceipt> {
        self.exchange.effects.lock().await.push(req.clone());
        if let Some(notify) = &self.exchange.submit_started {
            notify.notify_waiters();
        }
        if let Some(notify) = &self.exchange.release_submit {
            notify.notified().await;
        }
        Ok(OrderReceipt {
            order_id: "order-1".into(),
            client_order_id: req.client_order_id,
            status: OrderStatus::New,
        })
    }

    async fn cancel_order(&self, _instrument: &Instrument, _order_id: &str) -> Result<()> {
        if let Some(message) = &self.exchange.cancel_order_error {
            return Err(ExecutionPortError::cancel_outcome_unknown(message.clone()).into());
        }
        Ok(())
    }

    async fn cancel_all(&self, _instrument: &Instrument) -> Result<()> {
        Ok(())
    }

    async fn get_position(&self, _instrument: &Instrument) -> Result<Position> {
        self.exchange
            .get_position_calls
            .fetch_add(1, Ordering::SeqCst);
        if let Some(notify) = &self.exchange.get_position_started {
            notify.notify_waiters();
        }
        if let Some(notify) = &self.exchange.release_get_position {
            notify.notified().await;
        }
        Ok(self.exchange.position.lock().await.clone())
    }

    async fn get_open_orders(&self, _instrument: &Instrument) -> Result<Vec<ExchangeOrder>> {
        self.exchange
            .get_open_orders_calls
            .fetch_add(1, Ordering::SeqCst);
        Ok(self.exchange.open_orders.lock().await.clone())
    }
}

struct FakeAccountPort {
    exchange: Arc<FakeExchange>,
}

#[async_trait::async_trait]
impl AccountPort for FakeAccountPort {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &Instrument,
    ) -> Result<poise_engine::ports::AccountCapacitySnapshot> {
        Ok(poise_engine::ports::AccountCapacitySnapshot {
            max_increase_notional: 1_000_000.0,
        })
    }

    async fn subscribe_user_data(&self) -> Result<tokio::sync::mpsc::Receiver<UserDataEvent>> {
        let _ = &self.exchange;
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        Ok(receiver)
    }
}

#[derive(Default)]
pub(crate) struct MemoryRepository {
    snapshots: AsyncMutex<HashMap<String, poise_engine::snapshot::TrackRuntimeSnapshot>>,
    effects: AsyncMutex<Vec<PersistedTrackEffect>>,
    follow_up_retirements: AsyncMutex<HashMap<TrackId, Vec<FollowUpRetirementRequest>>>,
    next_effect_batch: AsyncMutex<u64>,
}

impl MemoryRepository {
    pub(crate) async fn seed_snapshot(
        &self,
        id: &str,
        snapshot: poise_engine::snapshot::TrackRuntimeSnapshot,
    ) {
        self.snapshots.lock().await.insert(id.to_string(), snapshot);
    }

    pub(crate) async fn seed_effect(&self, effect: PersistedTrackEffect) {
        self.effects.lock().await.push(effect);
    }

    pub(crate) async fn list_all_effects(&self) -> Vec<PersistedTrackEffect> {
        self.effects.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl TrackMutationStore for MemoryRepository {
    async fn save_transition_with_effect_status(
        &self,
        id: &str,
        state: &poise_engine::snapshot::TrackRuntimeSnapshot,
        _events: &[poise_core::events::DomainEvent],
        effects: &[TrackEffect],
        effect_status_update: Option<&EffectStatusUpdate>,
    ) -> Result<CommittedTrackWrite> {
        self.snapshots
            .lock()
            .await
            .insert(id.to_string(), state.clone());

        let now = Utc::now();
        let mut effect_store = self.effects.lock().await;
        let mut next_effect_batch = self.next_effect_batch.lock().await;
        *next_effect_batch += 1;
        let batch_id = next_effect_batch.to_string();
        let mut persisted_effects = Vec::new();
        for (sequence, effect) in effects.iter().enumerate() {
            if matches!(effect, TrackEffect::NoOp) {
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

        if let Some(effect_status_update) = effect_status_update {
            let effect = effect_store
                .iter_mut()
                .find(|effect| effect.effect_id == effect_status_update.effect_id)
                .ok_or_else(|| anyhow!("effect `{}` not found", effect_status_update.effect_id))?;
            effect.status = effect_status_update.status;
            effect.attempt_count += effect_status_update.attempt_delta;
            effect.last_error = effect_status_update.last_error.clone();
            effect.updated_at = now;
        }

        Ok(CommittedTrackWrite {
            track_id: TrackId::new(id),
            effects: persisted_effects,
        })
    }

    async fn load_track_state(
        &self,
        id: &str,
    ) -> Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
        Ok(self.snapshots.lock().await.get(id).cloned())
    }

    async fn list_track_events(&self, _id: &str) -> Result<Vec<poise_core::events::DomainEvent>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl TrackEffectStore for MemoryRepository {
    async fn list_dispatchable_effects(&self) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.status == EffectStatus::Pending)
            .cloned()
            .collect())
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
        Ok(self
            .effects
            .lock()
            .await
            .iter()
            .filter(|effect| effect.track_id == *track_id)
            .filter(|effect| effect.status == EffectStatus::Pending)
            .filter(|effect| matches!(effect.effect, TrackEffect::SubmitOrder { .. }))
            .cloned()
            .collect())
    }

    async fn list_pending_submit_effects_for_track_batch(
        &self,
        track_id: &TrackId,
        batch_id: &str,
    ) -> Result<Vec<PersistedTrackEffect>> {
        Ok(self
            .effects
            .lock()
            .await
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
#[async_trait::async_trait]
impl TrackQueryStore for MemoryRepository {
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
