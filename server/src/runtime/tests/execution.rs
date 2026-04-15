use super::*;

#[tokio::test]
async fn market_tick_submits_order_and_records_inventory_core_slot() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

    wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
    wait_until_instance(&fixture.state, |instance| {
        inventory_core_order(instance).is_some()
    })
    .await;

    let instance = current_instance(&fixture.state).await;
    let order = inventory_core_order(&instance).unwrap();
    assert_eq!(order.order_id.as_deref(), Some("order-1"));
    assert_eq!(
        instance
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(Exposure(4.0))
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn start_retries_transient_startup_failures() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
    fixture.exchange.fail_next_server_time_requests(2);
    fixture.exchange.fail_next_open_orders_requests(1);

    let handles = fixture.runtime.start().await.unwrap();

    assert_eq!(
        fixture
            .exchange
            .get_server_time_calls
            .load(Ordering::SeqCst),
        3
    );
    assert_eq!(
        fixture.exchange.get_position_calls.load(Ordering::SeqCst),
        2
    );
    assert_eq!(
        fixture
            .exchange
            .get_open_orders_calls
            .load(Ordering::SeqCst),
        2
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn account_monitor_task_triggers_immediate_refresh_and_periodic_refresh() {
    let fixture = runtime_fixture_with_account_refresh_interval(
        None,
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
        Duration::from_millis(25),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();

    wait_until(|| {
        fixture
            .exchange
            .get_account_summary_calls
            .load(Ordering::SeqCst)
            >= 3
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_preflight_marks_all_pending_submit_effects_not_only_dispatchable_ones() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
    let snapshot = fixture
        .state
        .manager()
        .read()
        .await
        .snapshot("BTCUSDT")
        .unwrap();
    let persisted = fixture
        .persistence
        .save_transition(
            "BTCUSDT",
            &snapshot,
            &[],
            &[
                TrackEffect::CancelAll {
                    instrument: btc_instrument(),
                },
                TrackEffect::SubmitOrder {
                    request: OrderRequest {
                        instrument: btc_instrument(),
                        side: Side::Buy,
                        price: 95.0,
                        quantity: test_config().base_qty_per_unit() * 4.0,
                        client_order_id: "startup-pending".into(),
                        reduce_only: false,
                    },
                    desired_exposure: Exposure(4.0),
                    submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
                },
            ],
        )
        .await
        .unwrap();

    let handles = fixture.runtime.start().await.unwrap();
    let startup_effects = fixture
        .state
        .submit_preflight
        .startup_pending_effect_ids()
        .await;
    assert!(startup_effects.contains(&persisted.effects[1].effect_id));

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_sampling_happens_after_startup_replay_before_effect_worker_runs() {
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        btc_position(0.0, 0.0),
        vec![],
        submit_started.clone(),
        release_submit.clone(),
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    let (user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::with_reconcile_intervals(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
        Duration::from_secs(1),
        Duration::from_secs(5),
    );

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let effect_id = persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist before start")
        .effect_id;
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));

    let handles = runtime.start().await.unwrap();
    submit_started.notified().await;
    let startup_effects = state.submit_preflight.startup_pending_effect_ids().await;
    release_submit.notify_waiters();

    assert!(startup_effects.contains(&effect_id));
    drop(price_sender);
    drop(user_sender);
    shutdown(handles).await;
}

#[tokio::test]
async fn effect_worker_executes_persisted_submit_order_and_marks_success() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let transition = fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(
        transition
            .effects
            .iter()
            .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
    );
    assert_eq!(
        fixture
            .persistence
            .list_dispatchable_effects()
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    let handles = fixture.runtime.start().await.unwrap();

    wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
    wait_until_async(|| {
        let persistence = Arc::clone(&fixture.persistence);
        async move {
            persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty()
        }
    })
    .await;

    let instance = current_instance(&fixture.state).await;
    assert_eq!(
        inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
        Some("order-1")
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn repeated_ticks_before_first_submit_are_absorbed_into_one_replacement_plan() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let first = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(matches!(
        first.effects.as_slice(),
        [ExecutionAction::SubmitOrder { .. }]
    ));

    let second = state.observe_market("BTCUSDT", 92.5).await.unwrap();
    assert!(
        second.effects.is_empty(),
        "new tick should update target only while first submit intent is pending"
    );

    timeout(Duration::from_secs(1), worker.run_once())
        .await
        .expect("cancel writeback recovery should finish promptly")
        .unwrap();

    let submitted = exchange.submitted_orders.lock().unwrap().clone();
    assert_eq!(submitted.len(), 1);
    assert!(matches!(
        submitted.as_slice(),
        [OrderRequest {
            side: Side::Buy,
            price,
            quantity,
            ..
        }] if (*price - 92.5).abs() < f64::EPSILON
            && (*quantity - test_config().base_qty_per_unit() * 6.0).abs() < f64::EPSILON
    ));
    assert!(
        persistence
            .list_dispatchable_effects()
            .await
            .unwrap()
            .is_empty(),
        "replacement submit should not leave duplicate pending submit effects behind"
    );
}

#[tokio::test]
async fn repeated_ticks_do_not_supersede_submit_effect_when_target_drift_stays_within_min_rebalance_units()
 {
    let exchange = Arc::new(FakeExchange::new(btc_position(2.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(2.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let first = state.observe_market("BTCUSDT", 96.5).await.unwrap();
    let (first_request, first_desired_exposure) = match first.effects.as_slice() {
        [
            ExecutionAction::SubmitOrder {
                request,
                desired_exposure,
                ..
            },
        ] => (request.clone(), desired_exposure.clone()),
        other => panic!("expected one submit effect, got {other:?}"),
    };

    let second = state.observe_market("BTCUSDT", 96.125).await.unwrap();
    assert!(
        second.effects.is_empty(),
        "small drift should not supersede the active submit intent"
    );

    worker.run_once().await.unwrap();

    let submitted = exchange.submitted_orders.lock().unwrap().clone();
    assert_eq!(submitted, vec![first_request.clone()]);
    assert!(exchange.canceled_order_ids.lock().unwrap().is_empty());

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Succeeded);

    let instance = current_instance(&state).await;
    assert!(instance.desired_exposure.is_some());
    assert_eq!(instance.desired_exposure, Some(first_desired_exposure.clone()));
    let order = inventory_core_order(&instance).expect("submit should become working");
    assert_eq!(order.client_order_id, first_request.client_order_id);
    assert_eq!(
        instance
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(first_desired_exposure.clone())
    );
    assert_eq!(order.order_id.as_deref(), Some("order-1"));
}

#[tokio::test]
async fn active_working_order_is_not_cancel_replaced_for_small_target_drift() {
    let exchange = Arc::new(FakeExchange::new(btc_position(2.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(2.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let first = state.observe_market("BTCUSDT", 96.5).await.unwrap();
    let first_desired_exposure = match first.effects.as_slice() {
        [
            ExecutionAction::SubmitOrder {
                desired_exposure, ..
            },
        ] => desired_exposure.clone(),
        other => panic!("expected one submit effect, got {other:?}"),
    };

    worker.run_once().await.unwrap();

    let second = state.observe_market("BTCUSDT", 96.125).await.unwrap();
    assert!(
        second.effects.is_empty(),
        "small drift should keep the active working order"
    );

    assert_eq!(
        exchange.submitted_orders.lock().unwrap().len(),
        1,
        "small drift should not create a replacement submit"
    );
    assert!(
        exchange.canceled_order_ids.lock().unwrap().is_empty(),
        "small drift should not cancel the active working order"
    );

    let instance = current_instance(&state).await;
    assert!(instance.desired_exposure.is_some());
    assert_eq!(instance.desired_exposure, Some(first_desired_exposure.clone()));
    let order = inventory_core_order(&instance).expect("working order should remain active");
    assert_eq!(
        instance
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(first_desired_exposure.clone())
    );
    assert_eq!(order.order_id.as_deref(), Some("order-1"));
}

#[tokio::test]
async fn partial_fill_does_not_cancel_replace_active_working_order_when_target_drift_stays_within_min_rebalance_units()
 {
    let exchange = Arc::new(FakeExchange::new(btc_position(2.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(2.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let first = state.observe_market("BTCUSDT", 96.5).await.unwrap();
    let first_desired_exposure = match first.effects.as_slice() {
        [
            ExecutionAction::SubmitOrder {
                desired_exposure, ..
            },
        ] => desired_exposure.clone(),
        other => panic!("expected one submit effect, got {other:?}"),
    };

    worker.run_once().await.unwrap();

    let first_order = inventory_core_order(&current_instance(&state).await)
        .unwrap()
        .clone();
    let remaining_quantity = first_order.quantity - 0.4;

    state
        .observe_position(
            "BTCUSDT",
            super::position_observation(&btc_position(2.4, 0.0)),
        )
        .await
        .unwrap();
    state
        .observe_order_with_absorb_result(
            "BTCUSDT",
            super::order_observation(&btc_exchange_order(
                first_order.order_id.as_deref().unwrap(),
                &first_order.client_order_id,
                Side::Buy,
                first_order.price,
                remaining_quantity,
                0.0,
                OrderStatus::PartiallyFilled,
            )),
        )
        .await
        .unwrap();
    let second = state.observe_market("BTCUSDT", 96.125).await.unwrap();
    assert!(
        second.effects.is_empty(),
        "partial fill followed by small target drift should keep the active working order"
    );

    assert_eq!(
        exchange.submitted_orders.lock().unwrap().len(),
        1,
        "partial fill followed by small target drift should not submit a replacement order"
    );
    assert!(
        exchange.canceled_order_ids.lock().unwrap().is_empty(),
        "partial fill followed by small target drift should not cancel the active order"
    );

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Succeeded);

    let instance = current_instance(&state).await;
    assert!(instance.desired_exposure.is_some());
    assert_eq!(instance.desired_exposure, Some(first_desired_exposure.clone()));
    let order = inventory_core_order(&instance).expect("working order should remain active");
    assert_eq!(order.client_order_id, first_order.client_order_id);
    assert_eq!(
        instance
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(first_desired_exposure.clone())
    );
    assert_eq!(order.status, OrderStatus::PartiallyFilled);
    assert!((order.quantity - remaining_quantity).abs() < 1e-9);
}

#[tokio::test]
async fn runtime_small_drift_does_not_loop_replacing_orders_once_round_is_active() {
    let clock = Arc::new(MutableClock(Arc::new(Mutex::new(test_server_time()))));
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(2.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture_with_options(
        Some(snapshot),
        btc_position(2.0, 0.0),
        vec![],
        test_budget(),
        RuntimeFixtureOptions {
            recovery_retry_interval: Duration::from_secs(60),
            audit_interval: Duration::from_secs(60),
            account_refresh_interval: Duration::from_secs(5),
            clock: clock.clone() as Arc<dyn ClockPort>,
        },
    )
    .await;
    let worker = EffectWorker::new(
        fixture.worker.clone(),
        fixture.exchange.execution_port(),
        fixture.exchange.account_port(),
        Duration::from_millis(10),
    );

    let first = fixture.state.observe_market("BTCUSDT", 96.5).await.unwrap();
    assert!(matches!(
        first.effects.as_slice(),
        [ExecutionAction::SubmitOrder { .. }]
    ));
    worker.run_once().await.unwrap();

    clock.set(test_server_time() + chrono::Duration::seconds(70));
    let second = fixture.state.observe_market("BTCUSDT", 96.4).await.unwrap();
    assert!(matches!(
        second.effects.as_slice(),
        [
            ExecutionAction::CancelOrder { .. },
            ExecutionAction::SubmitOrder { .. }
        ]
    ));
    worker.run_once().await.unwrap();

    clock.set(test_server_time() + chrono::Duration::seconds(71));
    let third = fixture
        .state
        .observe_market("BTCUSDT", 96.35)
        .await
        .unwrap();
    assert!(
        third.effects.is_empty(),
        "fresh replacement should not trigger another replacement on the next small drift"
    );
    assert_eq!(fixture.exchange.submitted_orders.lock().unwrap().len(), 2);
    assert_eq!(fixture.exchange.canceled_order_ids.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn effect_worker_restores_pending_effect_after_restart() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert_eq!(
        fixture
            .persistence
            .list_dispatchable_effects()
            .await
            .unwrap()
            .len(),
        1
    );

    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let restarted_runtime = ServerRuntime::new(
        fixture.state.runtime_state(),
        fixture.worker.effect_worker_state.clone(),
        RuntimePorts::new(
            fixture.exchange.execution_port(),
            Arc::new(FakeMarketData::new(price_receiver)) as Arc<dyn MarketDataPort>,
            fixture.exchange.account_port(),
            fixture.exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    let handles = restarted_runtime.start().await.unwrap();

    wait_until(|| fixture.exchange.submitted_orders.lock().unwrap().len() == 1).await;
    wait_until_async(|| {
        let persistence = Arc::clone(&fixture.persistence);
        async move {
            persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty()
        }
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn restarted_pending_submit_with_matching_live_order_is_recovered_without_duplicate_submit() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let persisted = fixture
        .persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist before restart");
    let TrackEffect::SubmitOrder { request, .. } = &persisted.effect else {
        panic!("expected persisted submit effect");
    };
    fixture.exchange.set_open_orders(vec![btc_exchange_order(
        "order-restored",
        &request.client_order_id,
        request.side,
        request.price,
        request.quantity,
        0.0,
        OrderStatus::New,
    )]);

    let handles = fixture.runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&fixture.persistence);
        async move {
            persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty()
        }
    })
    .await;

    let startup_effects = fixture
        .state
        .submit_preflight
        .startup_pending_effect_ids()
        .await;
    assert!(
        !startup_effects.contains(&persisted.effect_id),
        "recovered submit should be cleared from startup preflight tracking"
    );
    assert!(
        fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
        "matching live order should recover pending submit without duplicate submit"
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn attempted_submit_tracking_is_cleared_after_submit_success() {
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        btc_position(0.0, 0.0),
        vec![],
        submit_started.clone(),
        release_submit.clone(),
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    let (user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::with_reconcile_intervals(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
        Duration::from_secs(1),
        Duration::from_secs(5),
    );

    state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let effect_id = persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist before start")
        .effect_id;

    let handles = runtime.start().await.unwrap();
    submit_started.notified().await;
    assert!(state.submit_preflight.is_attempted(&effect_id).await);
    release_submit.notify_waiters();

    wait_until_async(|| {
        let state = state.clone();
        let effect_id = effect_id.clone();
        async move { !state.submit_preflight.is_attempted(&effect_id).await }
    })
    .await;

    drop(price_sender);
    drop(user_sender);
    shutdown(handles).await;
}

#[tokio::test]
async fn attempted_submit_tracking_is_cleared_after_submit_failure_or_supersede() {
    let exchange = Arc::new(FakeExchange::with_submit_error(
        btc_position(0.0, 0.0),
        vec![],
        "submit rejected",
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (price_sender, price_receiver) = mpsc::channel(8);
    let (user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::with_reconcile_intervals(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
        Duration::from_secs(1),
        Duration::from_secs(5),
    );

    state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let failed_effect_id = persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist before start")
        .effect_id;

    let handles = runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&persistence);
        let failed_effect_id = failed_effect_id.clone();
        async move {
            persistence.all_effects().await.into_iter().any(|effect| {
                effect.effect_id == failed_effect_id && effect.status == EffectStatus::Failed
            })
        }
    })
    .await;

    assert!(
        !state.submit_preflight.is_attempted(&failed_effect_id).await,
        "failed submit should be cleared from attempted preflight tracking"
    );

    drop(price_sender);
    drop(user_sender);
    shutdown(handles).await;

    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(6.0));
    snapshot.observed.strategy_price = Some(95.0);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(95.0);
    snapshot.observed.best_bid = Some(95.0);
    snapshot.observed.best_ask = Some(95.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            test_config().base_qty_per_unit() * 6.0,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    state
        .observe_position(
            "BTCUSDT",
            super::position_observation(&btc_position(0.0, 0.0)),
        )
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    state
        .submit_preflight
        .mark_submit_started("BTCUSDT:recovery:0")
        .await;
    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let restarted_runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            Arc::new(FakeMarketData::new(price_receiver)) as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    let handles = restarted_runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&persistence);
        async move {
            persistence.all_effects().await.into_iter().any(|effect| {
                effect.effect_id == "BTCUSDT:recovery:0"
                    && effect.status == EffectStatus::Superseded
            })
        }
    })
    .await;

    assert!(
        !state
            .submit_preflight
            .is_attempted("BTCUSDT:recovery:0")
            .await,
        "superseded submit should be cleared from attempted preflight tracking"
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_pending_tracking_is_cleared_on_track_effect_state_changed_notification() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(6.0));
    snapshot.observed.strategy_price = Some(95.0);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(95.0);
    snapshot.observed.best_bid = Some(95.0);
    snapshot.observed.best_ask = Some(95.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            test_config().base_qty_per_unit() * 6.0,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    state
        .observe_position(
            "BTCUSDT",
            super::position_observation(&btc_position(0.0, 0.0)),
        )
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let restarted_runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            Arc::new(FakeMarketData::new(price_receiver)) as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    let handles = restarted_runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&persistence);
        async move {
            persistence.all_effects().await.into_iter().any(|effect| {
                effect.effect_id == "BTCUSDT:recovery:0"
                    && effect.status == EffectStatus::Superseded
            })
        }
    })
    .await;

    let startup_effects = state.submit_preflight.startup_pending_effect_ids().await;
    assert!(
        !startup_effects.contains("BTCUSDT:recovery:0"),
        "track effect state change should clear startup pending submit tracking"
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn submit_preflight_reconciles_tracked_entries_after_external_effect_status_change() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let effect_id = fixture
        .persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist before runtime start")
        .effect_id;

    let handles = fixture.runtime.start().await.unwrap();

    wait_until_async(|| {
        let state = fixture.state.clone();
        let effect_id = effect_id.clone();
        async move {
            state
                .submit_preflight
                .startup_pending_effect_ids()
                .await
                .contains(&effect_id)
        }
    })
    .await;

    fixture
        .state
        .effect_service
        .complete_effect_failed("BTCUSDT", &effect_id, "manually retired in test")
        .await
        .unwrap();

    wait_until_async(|| {
        let state = fixture.state.clone();
        let effect_id = effect_id.clone();
        async move {
            !state
                .submit_preflight
                .startup_pending_effect_ids()
                .await
                .contains(&effect_id)
        }
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn submit_preflight_retries_after_transient_pending_effect_query_failure() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let task = fixture.runtime.spawn_submit_preflight_task(shutdown_rx);

    fixture
        .state
        .submit_preflight
        .seed_startup_pending_submit_effects(["stale-effect".to_string()])
        .await;
    fixture
        .persistence
        .fail_next_pending_submit_effect_queries(1);
    fixture
        .state
        .submit_preflight
        .mark_pending_submit_effects_dirty();

    wait_until_async(|| {
        let state = fixture.state.clone();
        async move {
            !state
                .submit_preflight
                .startup_pending_effect_ids()
                .await
                .contains("stale-effect")
        }
    })
    .await;

    let _ = shutdown_tx.send(true);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn failed_effect_does_not_roll_back_committed_snapshot() {
    let exchange = Arc::new(FakeExchange::with_submit_error(
        btc_position(0.0, 0.0),
        vec![],
        "submit rejected",
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(
        transition
            .effects
            .iter()
            .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
    );
    assert_eq!(
        persistence.list_dispatchable_effects().await.unwrap().len(),
        1
    );

    let handles = runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&persistence);
        async move {
            persistence
                .all_effects()
                .await
                .iter()
                .any(|effect| effect.status == EffectStatus::Failed)
        }
    })
    .await;

    let instance = current_instance(&state).await;
    assert_eq!(instance.desired_exposure, Some(Exposure(4.0)));
    assert!(inventory_core_order(&instance).is_none());

    shutdown(handles).await;
}

#[tokio::test]
async fn insufficient_margin_guard_activates_after_exchange_rejects_submit() {
    let exchange = Arc::new(FakeExchange::with_submit_error(
        btc_position(0.0, 0.0),
        vec![],
        r#"request POST /fapi/v1/order failed with status 400 Bad Request: {"code":-2019,"msg":"Margin is insufficient."}"#,
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(
        transition
            .effects
            .iter()
            .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
    );

    let handles = runtime.start().await.unwrap();

    wait_until_async(|| {
        let persistence = Arc::clone(&persistence);
        async move {
            persistence
                .list_dispatchable_effects()
                .await
                .unwrap()
                .is_empty()
        }
    })
    .await;

    let constraint = state.account_margin_guard.constraint_for(&btc_instrument());
    assert!(constraint.increase_blocked);
    assert_eq!(
        constraint.blocked_reason.as_deref(),
        Some("insufficient_margin")
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn insufficient_margin_guard_blocks_follow_up_submit_after_market_tick() {
    let exchange = Arc::new(FakeExchange::with_submit_error(
        btc_position(0.0, 0.0),
        vec![],
        r#"request POST /fapi/v1/order failed with status 400 Bad Request: {"code":-2019,"msg":"Margin is insufficient."}"#,
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (_price_sender, price_receiver) = mpsc::channel(8);
    let (_user_sender, _user_receiver) = mpsc::channel::<poise_engine::ports::UserDataEvent>(8);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let runtime = ServerRuntime::new(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            exchange.account_port(),
            exchange.metadata_port(),
            Arc::new(FixedClock(test_server_time())),
        ),
    );

    state.observe_market("BTCUSDT", 95.0).await.unwrap();

    let handles = runtime.start().await.unwrap();

    wait_until(|| {
        state
            .account_margin_guard
            .constraint_for(&btc_instrument())
            .increase_blocked
    })
    .await;

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();

    assert!(
        transition
            .events
            .iter()
            .any(|event| matches!(event, DomainEvent::RiskDenied { .. }))
    );
    assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
    assert_eq!(exchange.submitted_orders.lock().unwrap().len(), 1);

    let instance = current_instance(&state).await;
    assert!(instance.risk.account_capacity_constraint.increase_blocked);
    let source = TrackQueryService::new(
        persistence.clone() as Arc<dyn TrackQueryStore>,
        crate::test_support::test_prepared_registry("BTCUSDT"),
    )
    .load_track_detail_source(&TrackId::new("BTCUSDT"))
    .await
    .unwrap()
    .unwrap();
    let detail = state.projector.project_detail(&source);
    assert_eq!(
        detail.execution.execution_status,
        poise_protocol::ExecutionStatusView::AttentionRequired
    );

    shutdown(handles).await;
}

#[test]
fn venue_level_block_applies_to_symbols_added_after_block_activation() {
    let store = AccountMarginGuardStore::default();
    let eth_instrument = Instrument::new(Venue::Binance, "ETHUSDT");

    store.activate_insufficient_margin(
        &btc_instrument(),
        "insufficient_margin",
        test_server_time(),
    );
    store.update_snapshot(
        eth_instrument.clone(),
        AccountCapacitySnapshot {
            max_increase_notional: 500.0,
        },
    );

    let constraint = store.constraint_for(&eth_instrument);

    assert!(constraint.increase_blocked);
    assert_eq!(
        constraint.blocked_reason.as_deref(),
        Some("insufficient_margin")
    );
    assert_eq!(constraint.max_increase_notional, Some(500.0));
}

#[tokio::test]
async fn effect_worker_leaves_submitting_working_order_when_receipt_persistence_fails() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(FailOnReceiptPersistence::default());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(
        transition
            .effects
            .iter()
            .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
    );
    worker.run_once().await.unwrap();

    let instance = current_instance(&state).await;
    let order = inventory_core_order(&instance).expect("submit intent should remain durable");
    assert_eq!(order.order_id, None);
    assert_eq!(order.status, OrderStatus::Submitting);

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Pending);
}

#[tokio::test]
async fn effect_worker_skips_stale_submit_when_track_is_paused_before_execution() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;

    let transition = fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [ExecutionAction::SubmitOrder { .. }]
    ));

    fixture
        .state
        .command("BTCUSDT", TrackCommand::Pause)
        .await
        .unwrap();
    let handles = fixture.runtime.start().await.unwrap();
    wait_until_async(|| {
        let persistence = fixture.persistence.clone();
        async move {
            persistence.all_effects().await.iter().any(|effect| {
                effect.status == EffectStatus::Superseded
                    && matches!(effect.effect, ExecutionAction::SubmitOrder { .. })
            })
        }
    })
    .await;

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.desired_exposure, None);
    assert!(inventory_core_order(&instance).is_none());
    assert!(
        fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
        "paused track should not execute stale submit effects"
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn effect_worker_retires_stale_submit_until_first_tick_when_current_exposure_has_changed() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(4.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let (_state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:stale:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "stale".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 4.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    let submitted = exchange.submitted_orders.lock().unwrap().clone();
    assert!(
        submitted.is_empty(),
        "without a fresh tick, stale submit recovery should not derive a replacement order"
    );
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(
        effects
            .iter()
            .find(|effect| effect.effect_id == "BTCUSDT:stale:0")
            .map(|effect| effect.status),
        Some(EffectStatus::Superseded)
    );
}

#[tokio::test]
async fn effect_worker_retires_submit_until_first_tick_when_restored_quote_is_missing_even_if_quantity_rounding_breaks_reverse_exposure_math()
 {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let config = rounded_submit_test_config();
    let mut snapshot = test_snapshot_with_config(config.clone());
    snapshot.current_exposure = Exposure(2.0);
    snapshot.desired_exposure = Some(Exposure(3.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    snapshot.observed.strategy_price = Some(95.0);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(95.0);
    snapshot.observed.best_bid = Some(95.0);
    snapshot.observed.best_ask = Some(95.0);
    let (_state, worker_state) = test_launch_contexts_with_config(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
        config,
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:rounded:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "rounded".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 3.3,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(3.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    let submitted_orders = exchange.submitted_orders.lock().unwrap().clone();
    assert!(
        submitted_orders.is_empty(),
        "without a fresh tick, restored submit effects should wait instead of reusing stale quote-derived prices"
    );

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Superseded);
}

#[tokio::test]
async fn effect_worker_waits_for_exchange_state_when_receipt_snapshot_has_no_live_order_and_target_not_reached()
 {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(2.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("order-restored"),
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    assert!(
        exchange.submitted_orders.lock().unwrap().is_empty(),
        "receipt-backed recovery should wait for live exchange state instead of resubmitting"
    );
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Pending);
    let instance = current_instance(&state).await;
    assert_eq!(
        inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
        Some("order-restored")
    );
}

#[tokio::test]
async fn superseded_recovery_submit_waits_for_first_tick_before_replacement() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(6.0));
    snapshot.observed.strategy_price = Some(95.0);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(95.0);
    snapshot.observed.best_bid = Some(95.0);
    snapshot.observed.best_ask = Some(95.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            test_config().base_qty_per_unit() * 6.0,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();

    let transition = state
        .observe_position(
            "BTCUSDT",
            super::position_observation(&btc_position(0.0, 0.0)),
        )
        .await
        .unwrap();
    assert!(transition.effects.is_empty());
    assert_eq!(
        current_instance(&state).await.desired_exposure,
        Some(Exposure(6.0))
    );

    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    let submitted = exchange.submitted_orders.lock().unwrap().clone();
    assert!(
        submitted.is_empty(),
        "without a fresh tick, recovery should not synthesize a replacement submit"
    );
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(
        effects
            .iter()
            .find(|effect| effect.effect_id == "BTCUSDT:recovery:0")
            .map(|effect| effect.status),
        Some(EffectStatus::Superseded)
    );
}

#[tokio::test]
async fn effect_worker_keeps_receipt_backed_submit_pending_when_attention_required_is_active() {
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(6.0);
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("order-restored"),
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(22.5, 0.0),
        vec![],
        test_budget(),
    )
    .await;
    fixture
        .persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let handles = fixture.runtime.start().await.unwrap();

    assert!(
        fixture.exchange.submitted_orders.lock().unwrap().is_empty(),
        "attention_required should block duplicate submit attempts"
    );
    let effects = fixture.persistence.all_effects().await;
    assert_eq!(
        effects
            .iter()
            .find(|effect| effect.effect_id == "BTCUSDT:recovery:0")
            .map(|effect| effect.status),
        Some(EffectStatus::Pending)
    );
    let instance = current_instance(&fixture.state).await;
    assert!(inventory_core_order(&instance).is_none());
    assert_eq!(instance.current_exposure, Exposure(6.0));
    assert_eq!(
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref(),
        Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn effect_worker_supersedes_submit_when_target_is_reached_without_receipt_evidence() {
    let exchange = Arc::new(FakeExchange::new(btc_position(22.5, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    snapshot.current_exposure = Exposure(6.0);
    snapshot.desired_exposure = Some(Exposure(6.0));
    snapshot.observed.strategy_price = Some(92.5);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(92.5);
    snapshot.observed.best_bid = Some(92.5);
    snapshot.observed.best_ask = Some(92.5);
    let (_state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &snapshot, &[], &[])
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 92.5,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    assert!(
        exchange.submitted_orders.lock().unwrap().is_empty(),
        "recovered submit without receipt evidence should not resubmit"
    );
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Superseded);
}

#[tokio::test]
async fn effect_worker_does_not_submit_follow_up_effect_after_failed_cancel_in_same_batch() {
    let exchange = Arc::new(FakeExchange::with_cancel_order_error(
        btc_position(0.0, 0.0),
        vec![],
        "cancel order rejected",
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(4.0));
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("snapshot-1"),
            "snapshot-1",
            Side::Buy,
            94.0,
            0.25,
            Exposure(4.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot),
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let transition = state.observe_market("BTCUSDT", 90.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [
            ExecutionAction::CancelOrder { .. },
            ExecutionAction::SubmitOrder { .. }
        ]
    ));

    worker.run_once().await.unwrap();

    assert_eq!(
        exchange.canceled_order_ids.lock().unwrap().as_slice(),
        ["snapshot-1"]
    );
    assert!(
        exchange.submitted_orders.lock().unwrap().is_empty(),
        "submit should stay blocked behind failed cancel"
    );

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 2);
    assert_eq!(effects[0].status, EffectStatus::Failed);
    assert_eq!(effects[1].status, EffectStatus::Pending);
}

#[tokio::test]
async fn filled_order_after_failed_cancel_waits_for_fresh_tick_before_starting_new_lifecycle() {
    let exchange = Arc::new(FakeExchange::with_cancel_order_outcome_unknown(
        btc_position(-22.5, 0.0),
        vec![],
        "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(-6.0);
    snapshot.desired_exposure = Some(Exposure(-10.0));
    snapshot.observed.strategy_price = Some(105.0);
    snapshot.observed.strategy_price_status = poise_engine::runtime::StrategyPriceStatus::Live;
    snapshot.observed.mark_price = Some(105.0);
    snapshot.observed.best_bid = Some(105.0);
    snapshot.observed.best_ask = Some(105.0);
    set_executor_state(
        &mut snapshot,
        WorkingOrder {
            order_id: Some("order-large-sell".into()),
            client_order_id: "order-large-sell".into(),
            side: Side::Sell,
            price: 106.0,
            quantity: 15.0,
            status: OrderStatus::New,
            role: OrderRole::IncreaseInventory,
        },
        SlotState::Working,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot),
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let transition = state
        .observe_position(
            "BTCUSDT",
            super::position_observation(&btc_position(-22.5, 0.0)),
        )
        .await
        .unwrap();
    assert!(
        transition.effects.is_empty(),
        "without a fresh tick, filled position replay should not start a new quote-driven lifecycle"
    );

    worker.run_once().await.unwrap();

    let effects = persistence.all_effects().await;
    assert!(effects.is_empty());
    assert!(exchange.submitted_orders.lock().unwrap().is_empty());

    state
        .observe_order_with_absorb_result(
            "BTCUSDT",
            super::order_observation(&btc_exchange_order(
                "order-large-sell",
                "order-large-sell",
                Side::Sell,
                106.0,
                15.0,
                0.0,
                OrderStatus::Filled,
            )),
        )
        .await
        .unwrap();

    let effects_after_terminal_update = persistence.all_effects().await;
    assert!(effects_after_terminal_update.is_empty());
}

#[tokio::test]
async fn effect_worker_keeps_effect_pending_when_submit_cleanup_persistence_fails() {
    let exchange = Arc::new(FakeExchange::with_submit_error(
        btc_position(0.0, 0.0),
        vec![],
        "submit rejected",
    ));
    let persistence = Arc::new(FailOnSavePersistence::new(2));
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    let transition = state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert!(
        transition
            .effects
            .iter()
            .any(|effect| matches!(effect, ExecutionAction::SubmitOrder { .. }))
    );

    worker.run_once().await.unwrap();

    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Pending);
    assert_eq!(effects[0].attempt_count, 0);

    let instance = current_instance(&state).await;
    assert_eq!(
        inventory_core_order(&instance).map(|order| order.status),
        Some(OrderStatus::Submitting)
    );
}

#[tokio::test]
async fn cancel_success_writeback_failure_keeps_cancel_pending_and_recovers_exchange_state() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(FailOnSavePersistence::new(1));
    let snapshot = test_snapshot();
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(snapshot.clone()),
        test_budget(),
    )
    .await;
    persistence.seed_snapshot("BTCUSDT", snapshot).await;
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:cancel:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "cancel".into(),
            sequence: 0,
            effect: ExecutionAction::CancelOrder {
                instrument: btc_instrument(),
                order_id: "snapshot-1".into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    timeout(Duration::from_secs(1), worker.run_once())
        .await
        .expect("cancel writeback recovery should finish promptly")
        .unwrap();
    let effects = persistence.all_effects().await;
    let cancel_effect = effects
        .iter()
        .find(|effect| effect.effect_id == "BTCUSDT:cancel:0")
        .expect("cancel effect should remain persisted");
    assert_eq!(cancel_effect.status, EffectStatus::Pending);
    assert_eq!(exchange.get_position_calls.load(Ordering::SeqCst), 1);
    assert_eq!(exchange.get_open_orders_calls.load(Ordering::SeqCst), 1);
    let instance = current_instance(&state).await;
    assert_ne!(
        inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
        Some("snapshot-1"),
        "reconcile should retire the stale pre-cancel working order"
    );
}

#[tokio::test]
async fn recovered_submit_emits_effect_state_changed_notification() {
    let exchange = Arc::new(FakeExchange::new(
        btc_position(0.0, 0.0),
        vec![btc_exchange_order(
            "order-restored",
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            0.0,
            OrderStatus::New,
        )],
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let mut restored_snapshot = test_snapshot();
    set_executor_state(
        &mut restored_snapshot,
        working_order(
            Some("order-restored"),
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        Some(restored_snapshot),
        test_budget(),
    )
    .await;
    persistence
        .save_transition("BTCUSDT", &current_instance(&state).await, &[], &[])
        .await
        .unwrap();
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:recovery:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "recovery".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );
    state
        .submit_preflight
        .seed_startup_pending_submit_effects(["BTCUSDT:recovery:0".to_string()])
        .await;
    let mut receiver = state.notifications.subscribe();

    worker.run_once().await.unwrap();

    let mut saw_effect_state_changed = false;
    for _ in 0..3 {
        let event = timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event,
            poise_application::ApplicationNotification::TrackChanged { .. }
        ) {
            saw_effect_state_changed = true;
            break;
        }
    }

    assert!(saw_effect_state_changed);
}

#[tokio::test]
async fn receipt_persistence_failure_does_not_emit_spurious_effect_state_change() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(FailOnReceiptPersistence::default());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );
    let mut receiver = state.notifications.subscribe();

    state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let committed = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        committed,
        poise_application::ApplicationNotification::TrackChanged { .. }
    ));
    worker.run_once().await.unwrap();

    timeout(Duration::from_millis(100), receiver.recv())
        .await
        .expect_err(
            "receipt writeback failure without recovered state should not emit extra track changes",
        );
}

#[tokio::test]
async fn receipt_persistence_failure_keeps_submit_pending_for_recovery() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(FailOnReceiptPersistence::default());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    state.observe_market("BTCUSDT", 95.0).await.unwrap();
    let persisted = persistence
        .list_dispatchable_effects()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("pending submit effect should exist");
    let TrackEffect::SubmitOrder { request, .. } = &persisted.effect else {
        panic!("expected persisted submit effect");
    };
    exchange.set_open_orders(vec![btc_exchange_order(
        "order-restored",
        &request.client_order_id,
        request.side,
        request.price,
        request.quantity,
        0.0,
        OrderStatus::New,
    )]);

    worker.run_once().await.unwrap();

    let recovered = persistence
        .all_effects()
        .await
        .into_iter()
        .find(|effect| effect.effect_id == persisted.effect_id)
        .expect("submit effect should remain stored");
    assert_eq!(
        recovered.status,
        EffectStatus::Pending,
        "receipt writeback failure should keep the submit recoverable instead of failing it"
    );
}

#[tokio::test]
async fn effect_worker_keeps_effect_pending_while_submit_is_inflight() {
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        btc_position(0.0, 0.0),
        vec![],
        submit_started.clone(),
        release_submit.clone(),
    ));
    let persistence = Arc::new(MemoryPersistence::default());
    let (state, worker_state) = test_launch_contexts(
        exchange.metadata_port(),
        exchange.account_summary_port(),
        persistence.clone(),
        None,
        test_budget(),
    )
    .await;
    let worker = EffectWorker::new(
        worker_state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    state.observe_market("BTCUSDT", 95.0).await.unwrap();

    let task = tokio::spawn({
        let worker = worker.clone();
        async move { worker.run_once().await }
    });

    submit_started.notified().await;
    let effects = persistence.all_effects().await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].status, EffectStatus::Pending);

    release_submit.notify_waiters();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn effect_worker_keeps_effect_pending_when_loaded_track_is_missing_for_writeback() {
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let persistence = Arc::new(MemoryPersistence::default());
    persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:batch:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: ExecutionAction::CancelOrder {
                instrument: btc_instrument(),
                order_id: "order-1".into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let clock = Arc::new(FixedClock(
        Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
    ));
    let manager = TrackManager::new(clock);
    let (events, _) = broadcast::channel(16);
    let mutation_store: Arc<dyn TrackMutationStore> = persistence.clone();
    let effect_store: Arc<dyn TrackEffectStore> = persistence.clone();
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let services = build_test_application_services(
        manager,
        mutation_store,
        effect_store,
        events,
        account_margin_guard,
    );
    let state =
        build_effect_worker_test_context(&services, persistence.clone(), persistence.clone());
    let worker = EffectWorker::new(
        state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_millis(10),
    );

    worker.run_once().await.unwrap();

    let persisted = persistence.all_effects().await;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].status, EffectStatus::Pending);
    assert_eq!(persisted[0].last_error, None);
}
