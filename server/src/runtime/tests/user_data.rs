use super::*;

#[tokio::test]
async fn position_update_reconciles_actual_exposure_without_overwriting_target() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
    wait_until_instance(&fixture.state, |instance| {
        instance
            .desired_exposure
            .as_ref()
            .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
            .unwrap_or(false)
    })
    .await;

    fixture
        .user_sender
        .send(position_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            7.5,
            11.0,
        ))
        .await
        .unwrap();

    wait_until_instance(&fixture.state, |instance| {
        (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
            && instance
                .desired_exposure
                .as_ref()
                .map(|exposure| (exposure.0 - 4.0).abs() < f64::EPSILON)
                .unwrap_or(false)
    })
    .await;

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert_eq!(instance.desired_exposure, Some(Exposure(4.0)));
    assert!((instance.risk.unrealized_pnl - 11.0).abs() < f64::EPSILON);

    shutdown(handles).await;
}

#[tokio::test]
async fn position_update_reconciles_without_runtime_follow_up_command() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    drop(price_sender);
    let (user_sender, user_receiver) = mpsc::channel(8);
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
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(4.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    snapshot.observed.reference_price = Some(95.0);
    manager.restore_track_state(&snapshot).unwrap();
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();

    let (events, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
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
        build_test_account_monitor(exchange.clone() as Arc<dyn ExchangePort>, events).await,
        Arc::new(TrackProjector::new()),
    );
    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        exchange.clone() as Arc<dyn ExchangePort>,
        market_data as Arc<dyn MarketDataPort>,
    );

    let user_task = runtime.spawn_user_task(
        user_receiver,
        test_server_time(),
        runtime.shutdown_tx.subscribe(),
    );
    let save_count_before_event = persistence.save_transition_count();
    user_sender
        .send(position_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            7.5,
            11.0,
        ))
        .await
        .unwrap();

    wait_until_async(|| {
        let persistence = persistence.clone();
        async move { persistence.save_transition_count() == save_count_before_event + 1 }
    })
    .await;

    assert_eq!(
        persistence.save_transition_count() - save_count_before_event,
        1
    );
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects[0].effect,
        ExecutionAction::SubmitOrder { .. }
    ));
    assert!(exchange.submitted_orders.lock().unwrap().is_empty());

    user_task.abort();
    let _ = user_task.await;
}

#[tokio::test]
async fn position_update_submits_reconcile_without_waiting_for_new_tick() {
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(4.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    snapshot.observed.reference_price = Some(95.0);

    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    drop(price_sender);
    let (user_sender, user_receiver) = mpsc::channel(8);
    let market_data = Arc::new(FakeMarketData::without_user_receiver(price_receiver));
    let clock = Arc::new(FixedClock(test_server_time()));

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
    manager.restore_track_state(&snapshot).unwrap();
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();

    let (events, _) = broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
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
        build_test_account_monitor(exchange.clone() as Arc<dyn ExchangePort>, events).await,
        Arc::new(TrackProjector::new()),
    );
    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        exchange.clone() as Arc<dyn ExchangePort>,
        market_data as Arc<dyn MarketDataPort>,
    );

    let user_task = runtime.spawn_user_task(
        user_receiver,
        test_server_time(),
        runtime.shutdown_tx.subscribe(),
    );
    let effect_task = runtime.spawn_effect_task(runtime.shutdown_tx.subscribe());
    user_sender
        .send(position_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            7.5,
            11.0,
        ))
        .await
        .unwrap();

    wait_until(|| exchange.submitted_orders.lock().unwrap().len() == 1).await;
    wait_until_instance(&state, |instance| {
        inventory_core_order(instance).and_then(|order| order.order_id.as_deref())
            == Some("order-1")
    })
    .await;

    let submitted = exchange.submitted_orders.lock().unwrap().clone();
    assert_eq!(submitted[0].side, Side::Buy);
    assert_eq!(submitted[0].quantity, 7.5);

    user_task.abort();
    let _ = user_task.await;
    effect_task.abort();
    let _ = effect_task.await;
}

#[tokio::test]
async fn position_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event() {
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(0.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    snapshot.observed.reference_price = Some(100.0);
    snapshot.risk.unrealized_pnl = 0.0;

    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    let mut receiver = fixture.state.notifications.subscribe();
    fixture
        .user_sender
        .send(position_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            0.0,
            11.0,
        ))
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        poise_application::ApplicationNotification::TrackChanged { .. }
    ));

    shutdown(handles).await;
}

#[tokio::test]
async fn order_update_clears_inventory_core_slot_on_terminal_status() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
    wait_until_instance(&fixture.state, |instance| {
        inventory_core_order(instance)
            .and_then(|order| order.order_id.as_deref())
            .is_some()
    })
    .await;

    let order = inventory_core_order(&current_instance(&fixture.state).await)
        .unwrap()
        .clone();
    fixture
        .exchange
        .set_position(btc_position(order.quantity, 0.0));

    fixture
        .user_sender
        .send(order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                &order.order_id.clone().unwrap(),
                &order.client_order_id,
                Side::Buy,
                order.price,
                order.quantity,
                0.0,
                OrderStatus::Filled,
            ),
        ))
        .await
        .unwrap();

    wait_until_instance(&fixture.state, |instance| {
        inventory_core_order(instance).is_none()
    })
    .await;
    assert_eq!(fixture.exchange.submitted_orders.lock().unwrap().len(), 1);

    shutdown(handles).await;
}

#[tokio::test]
async fn terminal_order_update_reconciles_without_waiting_for_new_tick() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();
    wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;

    let order = inventory_core_order(&current_instance(&fixture.state).await)
        .unwrap()
        .clone();

    fixture
        .user_sender
        .send(order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                &order.order_id.clone().unwrap(),
                &order.client_order_id,
                Side::Buy,
                order.price,
                order.quantity,
                0.0,
                OrderStatus::Canceled,
            ),
        ))
        .await
        .unwrap();

    wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 2).await;
    wait_until_instance(&fixture.state, |instance| {
        inventory_core_order(instance).and_then(|working_order| working_order.order_id.as_deref())
            == Some("order-2")
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn terminal_order_update_broadcasts_snapshot_updated_when_reconcile_emits_no_domain_event() {
    let mut snapshot = TrackRuntimeSnapshot {
        track_id: TrackId::new("BTCUSDT"),
        instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
        config: test_config(),
        status: TrackStatus::Active,
        current_exposure: Exposure(0.0),
        desired_exposure: Some(Exposure(0.0)),
        manual_target_override: None,
        executor_state: ExecutorState::empty(test_server_time()),
        replacement_gate_reason: None,
        ledger_state: Default::default(),
        risk: RiskState::default(),
        observed: poise_engine::snapshot::ObservedState {
            reference_price: Some(100.0),
            out_of_band_since: None,
            last_tick_at: None,
            market_data_stale_since: None,
        },
    };
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("order-1"),
            "order-1",
            Side::Buy,
            100.0,
            0.1,
            Exposure(0.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let open_orders = vec![ExchangeOrder {
        instrument: btc_instrument(),
        order_id: "order-1".into(),
        client_order_id: "order-1".into(),
        side: Side::Buy,
        price: 100.0,
        qty: 0.1,
        realized_pnl: 0.0,
        status: OrderStatus::New,
    }];
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(0.0, 0.0),
        open_orders,
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    let mut receiver = fixture.state.notifications.subscribe();
    fixture
        .user_sender
        .send(order_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            btc_exchange_order(
                "order-1",
                "order-1",
                Side::Buy,
                100.0,
                0.1,
                0.0,
                OrderStatus::Canceled,
            ),
        ))
        .await
        .unwrap();

    let event = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        poise_application::ApplicationNotification::TrackChanged { .. }
    ));

    shutdown(handles).await;
}
