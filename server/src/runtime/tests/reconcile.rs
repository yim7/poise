use super::*;

#[tokio::test]
async fn apply_user_data_event_preserves_write_service_mutation_error_kind() {
    let manager = TrackManager::new(Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
    )));
    let persistence = Arc::new(MemoryPersistence::default());
    let (events, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let services = build_test_application_services(
        manager,
        persistence.clone() as Arc<dyn TrackMutationStore>,
        persistence.clone() as Arc<dyn TrackEffectStore>,
        events.clone(),
        account_margin_guard.clone(),
    );
    let state = build_runtime_test_context(
        &services,
        persistence.clone() as Arc<dyn TrackMutationStore>,
        persistence.clone() as Arc<dyn TrackEffectStore>,
        build_test_account_monitor(
            Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![])),
            events,
        )
        .await,
        Arc::new(TrackProjector::new()),
    );

    let error = super::apply_user_data_event(
        &state,
        &(Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![])) as Arc<dyn ExchangePort>),
        "missing-track",
        position_event_at(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 1).unwrap(),
            1.0,
            0.0,
        ),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        TrackMutationError::Mutation(_)
    ));
}

#[tokio::test]
async fn apply_user_data_event_persists_track_ledger_event_atomically() {
    let exchange = Arc::new(FakeExchange::new(btc_position(15.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.observed.reference_price = Some(95.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("fill-1"),
            "fill-1",
            Side::Buy,
            94.5,
            test_config().base_qty_per_unit() * 2.0,
            Exposure(4.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        persistence.clone(),
        Some(snapshot),
        test_budget(),
    )
    .await;

    super::apply_user_data_event(
        &state,
        &(exchange.clone() as Arc<dyn ExchangePort>),
        "BTCUSDT",
        UserDataEvent {
            event_time: test_server_time() + chrono::Duration::milliseconds(1),
            payload: UserDataPayload::TrackLedger(TrackLedgerUpdate {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                event: TrackLedgerEvent::Execution(ExecutionLedgerUpdate {
                    order_update: OrderObservation {
                        order_id: "fill-1".into(),
                        client_order_id: "fill-1".into(),
                        side: Side::Buy,
                        price: 94.5,
                        quantity: 7.5,
                        realized_pnl: 12.34,
                        status: OrderStatus::Filled,
                    },
                    ledger_deltas: vec![
                        LedgerDelta::GrossRealizedPnl(12.34),
                        LedgerDelta::TradingFee(3.2),
                    ],
                    ledger_gaps: vec![],
                }),
            }),
        },
    )
    .await
    .unwrap();

    assert_eq!(persistence.save_transition_count.load(Ordering::SeqCst), 1);
    let instance = current_instance(&state).await;
    assert!((instance.ledger_state.gross_realized_pnl_cumulative - 12.34).abs() < f64::EPSILON);
    assert!((instance.ledger_state.trading_fee_cumulative - 3.2).abs() < f64::EPSILON);
    assert!(inventory_core_order(&instance).is_none());
}

#[tokio::test]
async fn filled_order_update_marks_track_stale_without_immediate_reconcile() {
    let exchange = Arc::new(FakeExchange::new(btc_position(15.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(4.0));
    snapshot.observed.reference_price = Some(95.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("fill-1"),
            "fill-1",
            Side::Buy,
            94.5,
            test_config().base_qty_per_unit() * 2.0,
            Exposure(4.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        persistence,
        Some(snapshot),
        test_budget(),
    )
    .await;

    super::apply_user_data_event(
        &state,
        &(exchange.clone() as Arc<dyn ExchangePort>),
        "BTCUSDT",
        order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                "fill-1",
                "fill-1",
                Side::Buy,
                94.5,
                7.5,
                0.0,
                OrderStatus::Filled,
            ),
        ),
    )
    .await
    .unwrap();

    let instance = current_instance(&state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert!(inventory_core_order(&instance).is_none());
    assert!(state.exchange_freshness.is_stale("BTCUSDT").await);
    assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 0);
    assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn successful_exchange_sync_clears_stale_state() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        Arc::new(MemoryPersistence::default()),
        None,
        test_budget(),
    )
    .await;
    state
        .exchange_freshness
        .mark_stale("BTCUSDT", ExchangeFreshnessReason::FilledAwaitingSync)
        .await;

    super::sync_exchange_state_from_exchange(
        &state,
        &(exchange.clone() as Arc<dyn ExchangePort>),
        "BTCUSDT",
        &btc_instrument(),
        ExchangeSyncMode::RecoverAndReconcile,
    )
    .await
    .unwrap();

    assert!(!state.exchange_freshness.is_stale("BTCUSDT").await);
}

#[tokio::test]
async fn successful_exchange_sync_does_not_clear_newer_stale_fact() {
    let get_position_started = Arc::new(Notify::new());
    let release_get_position = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_get_position(
        btc_position(0.0, 0.0),
        vec![],
        get_position_started.clone(),
        release_get_position.clone(),
    ));
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        Arc::new(MemoryPersistence::default()),
        None,
        test_budget(),
    )
    .await;
    state
        .exchange_freshness
        .mark_stale("BTCUSDT", ExchangeFreshnessReason::FilledAwaitingSync)
        .await;

    let task = tokio::spawn({
        let state = state.clone();
        let exchange = exchange.clone() as Arc<dyn ExchangePort>;
        async move {
            super::sync_exchange_state_from_exchange(
                &state,
                &exchange,
                "BTCUSDT",
                &btc_instrument(),
                ExchangeSyncMode::RecoverAndReconcile,
            )
            .await
        }
    });

    get_position_started.notified().await;
    state
        .exchange_freshness
        .mark_stale("BTCUSDT", ExchangeFreshnessReason::SubmitOutcomeUnknown)
        .await;
    release_get_position.notify_waiters();
    task.await.unwrap().unwrap();

    assert!(state.exchange_freshness.is_stale("BTCUSDT").await);
}

#[tokio::test]
async fn stale_live_user_event_does_not_rollback_state_after_start() {
    let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture
        .user_sender
        .send(position_event_at(
            test_server_time() - chrono::Duration::milliseconds(1),
            3.75,
            9.0,
        ))
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert!((instance.risk.unrealized_pnl - 3.0).abs() < f64::EPSILON);

    shutdown(handles).await;
}

#[tokio::test]
async fn filled_order_updates_realized_pnl_and_trips_daily_loss_cap() {
    let fixture = runtime_fixture(
        None,
        btc_position(7.5, 0.0),
        vec![],
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -10.0,
            stop_loss_pct: 10.0,
        },
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.exchange.set_position(btc_position(0.0, 0.0));
    fixture
        .user_sender
        .send(order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                "fill-1",
                "fill-1",
                Side::Sell,
                95.0,
                7.5,
                -20.0,
                OrderStatus::Filled,
            ),
        ))
        .await
        .unwrap();

    wait_until_instance(&fixture.state, |instance| {
        (instance.risk.realized_pnl_today + 20.0).abs() < f64::EPSILON
    })
    .await;

    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let submitted = fixture.exchange.submitted_orders.lock().unwrap().clone();
    assert!(submitted.is_empty());
    assert_eq!(
        current_instance(&fixture.state).await.desired_exposure,
        Some(Exposure(0.0))
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn unabsorbed_order_update_marks_stale_and_triggers_immediate_reconcile() {
    let get_position_started = Arc::new(Notify::new());
    let release_get_position = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_get_position(
        btc_position(0.0, 0.0),
        vec![],
        get_position_started.clone(),
        release_get_position.clone(),
    ));
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        Arc::new(MemoryPersistence::default()),
        None,
        test_budget(),
    )
    .await;

    let task = tokio::spawn({
        let state = state.clone();
        let exchange = exchange.clone() as Arc<dyn ExchangePort>;
        async move {
            super::apply_user_data_event(
                &state,
                &exchange,
                "BTCUSDT",
                order_event_at(
                    test_server_time() + chrono::Duration::milliseconds(1),
                    btc_exchange_order(
                        "untracked-live-order",
                        "untracked-live-order",
                        Side::Buy,
                        95.0,
                        1.0,
                        0.0,
                        OrderStatus::New,
                    ),
                ),
            )
            .await
        }
    });

    get_position_started.notified().await;
    assert!(state.exchange_freshness.is_stale("BTCUSDT").await);

    release_get_position.notify_waiters();
    task.await.unwrap().unwrap();

    assert!(!state.exchange_freshness.is_stale("BTCUSDT").await);
    assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
    assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unabsorbed_order_update_triggers_immediate_reconcile() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
    let handles = fixture.runtime.start().await.unwrap();
    let position_calls_before = fixture.exchange.get_position_calls.load(Ordering::SeqCst);
    let open_orders_calls_before = fixture
        .exchange
        .get_open_orders_calls
        .load(Ordering::SeqCst);

    fixture
        .user_sender
        .send(order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                "untracked-live-order",
                "untracked-live-order",
                Side::Buy,
                95.0,
                1.0,
                0.0,
                OrderStatus::New,
            ),
        ))
        .await
        .unwrap();

    wait_until(|| {
        fixture.exchange.get_position_calls.load(Ordering::SeqCst) > position_calls_before
            && fixture
                .exchange
                .get_open_orders_calls
                .load(Ordering::SeqCst)
                > open_orders_calls_before
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn immediate_reconcile_requests_are_single_flight_per_track() {
    let get_position_started = Arc::new(Notify::new());
    let release_get_position = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_get_position(
        btc_position(0.0, 0.0),
        vec![],
        get_position_started.clone(),
        release_get_position.clone(),
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let state = test_state(
        exchange.clone() as Arc<dyn ExchangePort>,
        persistence,
        None,
        test_budget(),
    )
    .await;
    let instrument = btc_instrument();

    let first = tokio::spawn({
        let state = state.clone();
        let exchange = exchange.clone() as Arc<dyn ExchangePort>;
        let instrument = instrument.clone();
        async move {
            super::enqueue_reconcile_request(
                &state,
                &exchange,
                crate::order_outcome::ReconcileRequest {
                    track_id: "BTCUSDT".into(),
                    reason: crate::order_outcome::ReconcileReason::SyncAfterSubmitOutcomeUnknown,
                },
                &instrument,
            )
            .await
        }
    });

    get_position_started.notified().await;

    let second = tokio::spawn({
        let state = state.clone();
        let exchange = exchange.clone() as Arc<dyn ExchangePort>;
        let instrument = instrument.clone();
        async move {
            super::enqueue_reconcile_request(
                &state,
                &exchange,
                crate::order_outcome::ReconcileRequest {
                    track_id: "BTCUSDT".into(),
                    reason: crate::order_outcome::ReconcileReason::SyncAfterCancelOutcomeUnknown,
                },
                &instrument,
            )
            .await
        }
    });

    sleep(Duration::from_millis(50)).await;
    assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);

    release_get_position.notify_waiters();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
}

#[tokio::test]
async fn normal_track_low_frequency_reconcile_discovers_untracked_live_orders_without_restart() {
    let fixture = runtime_fixture_with_intervals(
        None,
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
        Duration::from_secs(1),
        Duration::from_millis(50),
    )
    .await;
    let handles = fixture.runtime.start().await.unwrap();

    fixture.exchange.set_open_orders(vec![btc_exchange_order(
        "live-1",
        "unexpected-live-1",
        Side::Buy,
        94.5,
        0.25,
        0.0,
        OrderStatus::New,
    )]);

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn runtime_start_fails_when_user_data_subscription_cannot_be_created() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    drop(price_sender);
    let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
    let clock = Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
    ));

    let mut manager = TrackManager::new(clock);
    manager
        .add_track(
            TrackId::new("BTCUSDT"),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            test_config(),
            test_budget(),
            exchange.exchange_info.rules.clone(),
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
    let (state, worker_state) = build_runtime_and_effect_worker_test_contexts(
        &services,
        persistence.clone(),
        persistence.clone(),
        build_test_account_monitor(exchange.clone() as Arc<dyn ExchangePort>, events).await,
        Arc::new(TrackProjector::new()),
    );

    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        exchange as Arc<dyn ExchangePort>,
        market_data as Arc<dyn MarketDataPort>,
    );

    let error = runtime.start().await.err().unwrap();
    assert!(error.to_string().contains("missing test user receiver"));
}
