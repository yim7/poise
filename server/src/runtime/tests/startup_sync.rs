use super::*;

#[tokio::test]
async fn startup_sync_restores_claimed_live_order_before_replanning() {
    let snapshot = test_snapshot();
    let live_order = btc_exchange_order(
        "snapshot-1",
        "snapshot-1",
        Side::Buy,
        94.5,
        0.25,
        0.0,
        OrderStatus::New,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![live_order],
        test_budget(),
    )
    .await;
    let save_count_before_start = fixture.persistence.save_transition_count();

    fixture.runtime.startup_sync().await.unwrap();
    assert_eq!(
        fixture.persistence.save_transition_count() - save_count_before_start,
        1,
        "startup sync should persist live exchange state through a single write path"
    );

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert_eq!(instance.desired_exposure, Some(Exposure(4.0)));
    assert_eq!(
        instance.observed.out_of_band_since,
        Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap())
    );
    let executor_state = &instance.executor_state;
    assert_eq!(
        executor_state.slots.as_slice(),
        [poise_engine::runtime::ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state: SlotState::Working,
            working_order: Some(poise_engine::runtime::WorkingOrder {
                order_id: Some("snapshot-1".into()),
                client_order_id: "snapshot-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                status: OrderStatus::New,
                role: OrderRole::IncreaseInventory,
            }),
        }]
    );
    let effects = fixture.persistence.all_effects().await;
    assert!(effects.iter().any(|effect| {
        matches!(
            &effect.effect,
            ExecutionAction::CancelOrder { order_id, .. } if order_id == "snapshot-1"
        )
    }));
    assert!(effects.iter().any(|effect| {
        matches!(
            &effect.effect,
            ExecutionAction::SubmitOrder {
                request,
                desired_exposure,
                ..
            }
                if request.client_order_id.starts_with("BTCUSDT-")
                    && (request.price - 95.0).abs() < f64::EPSILON
                    && (request.quantity - 7.5).abs() < f64::EPSILON
                    && *desired_exposure == Exposure(4.0)
        )
    }));
}

#[tokio::test]
async fn startup_sync_replans_even_when_pending_submit_effect_is_present() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "snapshot-1",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
    )
    .await;
    fixture
        .persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:startup:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "startup".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "snapshot-1".into(),
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

    fixture.runtime.startup_sync().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    assert_eq!(
        inventory_core_order(&instance).map(|order| order.client_order_id.starts_with("BTCUSDT-")),
        Some(true)
    );

    let effects = fixture.persistence.all_effects().await;
    assert!(effects.iter().any(|effect| {
        matches!(
            &effect.effect,
            ExecutionAction::SubmitOrder {
                request,
                desired_exposure,
                ..
            }
                if request.client_order_id.starts_with("BTCUSDT-")
                    && (request.price - 95.0).abs() < f64::EPSILON
                    && (request.quantity - 15.0).abs() < f64::EPSILON
                    && *desired_exposure == Exposure(4.0)
        )
    }));
}

#[tokio::test]
async fn startup_sync_does_not_duplicate_matching_pending_submit_effect() {
    let mut snapshot = test_snapshot();
    snapshot.current_exposure = Exposure(0.0);
    snapshot.desired_exposure = Some(Exposure(6.0));
    snapshot.observed.reference_price = Some(92.5);
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            92.5,
            22.5,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
    )
    .await;
    fixture
        .persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:startup:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "startup".into(),
            sequence: 0,
            effect: ExecutionAction::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 92.5,
                    quantity: 22.5,
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

    fixture.runtime.startup_sync().await.unwrap();

    let pending_effects = fixture
        .persistence
        .list_dispatchable_effects()
        .await
        .unwrap();
    assert_eq!(pending_effects.len(), 1);
    assert!(matches!(
        pending_effects.as_slice(),
        [PersistedTrackEffect {
            effect:
                ExecutionAction::SubmitOrder {
                    request,
                    desired_exposure,
                    ..
                },
            ..
        }] if request.client_order_id == "BTCUSDT-reconcile"
            && (request.price - 92.5).abs() < f64::EPSILON
            && (request.quantity - 22.5).abs() < f64::EPSILON
            && *desired_exposure == Exposure(6.0)
    ));
}

#[tokio::test]
async fn startup_sync_marks_attention_required_when_live_order_cannot_be_claimed() {
    let mut snapshot = test_snapshot();
    snapshot.desired_exposure = Some(Exposure(0.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![btc_exchange_order(
            "live-1",
            "unexpected-live",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        )],
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(0.0));
    assert_eq!(instance.desired_exposure, Some(Exposure(0.0)));
    assert_eq!(
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref(),
        Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    );
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_sync_rebuilds_inventory_core_slot_when_exchange_has_no_open_orders() {
    let fixture = runtime_fixture(
        Some(test_snapshot()),
        btc_position(7.5, 3.0),
        vec![],
        test_budget(),
    )
    .await;

    fixture.runtime.startup_sync().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert_eq!(instance.desired_exposure, Some(Exposure(4.0)));
    assert_eq!(
        inventory_core_order(&instance).map(|order| order.client_order_id.starts_with("BTCUSDT-")),
        Some(true)
    );
    assert_ne!(
        inventory_core_order(&instance).and_then(|order| order.order_id.as_deref()),
        Some("snapshot-1")
    );
}

#[tokio::test]
async fn startup_sync_rebuilds_submit_pending_slot_to_current_plan_before_follow_up_sync() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![],
        test_budget(),
    )
    .await;
    fixture
        .persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:startup:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "startup".into(),
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
    fixture.runtime.startup_sync().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    let order = inventory_core_order(&instance).expect("expected submit pending working order");
    assert!(order.client_order_id.starts_with("BTCUSDT-"));
    assert_eq!(order.order_id, None);
    assert_eq!(order.side, Side::Buy);
    assert_eq!(order.price, 95.0);
    assert_eq!(order.quantity, 7.5);
    assert_eq!(
        instance
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(Exposure(4.0))
    );
    assert_eq!(order.status, OrderStatus::Submitting);

    let transition = fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
}

#[tokio::test]
async fn startup_sync_marks_attention_required_when_receipt_backed_submit_has_no_live_order() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("receipt-1"),
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
        btc_position(7.5, 3.0),
        vec![],
        test_budget(),
    )
    .await;
    fixture
        .persistence
        .seed_effect(PersistedTrackEffect {
            effect_id: "BTCUSDT:startup:0".into(),
            track_id: TrackId::new("BTCUSDT"),
            batch_id: "startup".into(),
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

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    })
    .await;
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_sync_clears_orphaned_submit_pending_slot_without_effect() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            None,
            "BTCUSDT-reconcile",
            Side::Buy,
            94.0,
            0.25,
            Exposure(6.0),
            OrderStatus::Submitting,
        ),
        SlotState::SubmitPending,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![],
        test_budget(),
    )
    .await;

    fixture.runtime.startup_sync().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    assert_eq!(
        inventory_core_order(&instance).map(|order| order.client_order_id.starts_with("BTCUSDT-")),
        Some(true)
    );

    let transition = fixture.state.observe_market("BTCUSDT", 95.0).await.unwrap();
    assert_eq!(transition.effects, vec![ExecutionAction::NoOp]);
}

#[tokio::test]
async fn startup_sync_rebuilds_multiple_live_open_orders_when_they_match_distinct_slots() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("order-a"),
            "client-a",
            Side::Buy,
            94.5,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    snapshot.executor_state.slots.push(ExecutionSlot {
        slot: OrderSlot::new("inventory_followup"),
        state: SlotState::Working,
        working_order: Some(working_order(
            Some("order-b"),
            "client-b",
            Side::Sell,
            95.5,
            0.15,
            Exposure(2.0),
            OrderStatus::PartiallyFilled,
        )),
    });
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![
            btc_exchange_order(
                "order-b",
                "client-b",
                Side::Sell,
                95.5,
                0.15,
                0.0,
                OrderStatus::New,
            ),
            btc_exchange_order(
                "order-a",
                "client-a",
                Side::Buy,
                94.5,
                0.25,
                0.0,
                OrderStatus::PartiallyFilled,
            ),
        ],
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();

    assert!(
        fixture
            .exchange
            .cancel_all_symbols
            .lock()
            .unwrap()
            .is_empty()
    );
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());
    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert!(
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .is_none()
    );
    assert_eq!(instance.executor_state.slots.len(), 2);
    assert_eq!(
        instance.executor_state.slots[0]
            .working_order
            .as_ref()
            .and_then(|order| order.order_id.as_deref()),
        Some("order-a")
    );
    assert_eq!(
        instance.executor_state.slots[1]
            .working_order
            .as_ref()
            .and_then(|order| order.order_id.as_deref()),
        Some("order-b")
    );

    shutdown(handles).await;
}

#[tokio::test]
async fn shutdown_cancels_orders_and_persists_final_exchange_state() {
    let mut snapshot = test_snapshot();
    set_executor_state(
        &mut snapshot,
        working_order(
            Some("live-1"),
            "live-1",
            Side::Buy,
            94.5,
            0.25,
            Exposure(6.0),
            OrderStatus::New,
        ),
        SlotState::Working,
    );
    let fixture = runtime_fixture(
        Some(snapshot),
        btc_position(7.5, 3.0),
        vec![btc_exchange_order(
            "live-1",
            "live-1",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        )],
        test_budget(),
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();

    fixture.runtime.shutdown(handles).await;

    assert_eq!(
        fixture
            .exchange
            .cancel_all_symbols
            .lock()
            .unwrap()
            .as_slice(),
        ["BTCUSDT"]
    );
    let snapshot = fixture
        .persistence
        .load_track_state("BTCUSDT")
        .await
        .unwrap()
        .expect("final snapshot should be persisted");
    assert_eq!(snapshot.current_exposure, Exposure(2.0));
    assert_eq!(snapshot.executor_state.diagnostics.recovery_anomaly, None);
    assert_eq!(
        snapshot.executor_state.slots,
        vec![ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }]
    );
}

#[tokio::test]
async fn recovery_task_resyncs_recovery_anomaly_automatically_without_user_data() {
    let mut snapshot = test_snapshot();
    snapshot.desired_exposure = Some(Exposure(0.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture_with_recovery_retry_interval(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![btc_exchange_order(
            "live-1",
            "unexpected-live",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        )],
        test_budget(),
        Duration::from_millis(50),
    )
    .await;

    let RuntimeHandles {
        market_task,
        user_task,
        effect_task,
        recovery_task,
        account_task,
    } = fixture.runtime.start().await.unwrap();
    market_task.abort();
    let _ = market_task.await;
    effect_task.abort();
    let _ = effect_task.await;

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    })
    .await;
    assert_eq!(
        fixture.exchange.get_position_calls.load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        fixture
            .exchange
            .get_open_orders_calls
            .load(Ordering::SeqCst),
        1
    );

    wait_until(|| {
        fixture
            .exchange
            .get_open_orders_calls
            .load(Ordering::SeqCst)
            >= 2
    })
    .await;
    assert!(fixture.exchange.get_position_calls.load(Ordering::SeqCst) >= 2);
    assert!(
        fixture
            .exchange
            .get_open_orders_calls
            .load(Ordering::SeqCst)
            >= 2
    );

    fixture.exchange.open_orders.lock().unwrap().clear();

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            .is_none()
    })
    .await;
    assert!(fixture.exchange.get_position_calls.load(Ordering::SeqCst) >= 3);
    assert!(
        fixture
            .exchange
            .get_open_orders_calls
            .load(Ordering::SeqCst)
            >= 3
    );
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    recovery_task.abort();
    let _ = recovery_task.await;
    account_task.abort();
    let _ = account_task.await;
    user_task.abort();
    let _ = user_task.await;
}

#[tokio::test]
async fn recovery_task_cancels_unknown_live_orders_automatically() {
    let mut snapshot = test_snapshot();
    snapshot.desired_exposure = Some(Exposure(0.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture_with_recovery_retry_interval(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![
            btc_exchange_order(
                "live-1",
                "unexpected-live-1",
                Side::Buy,
                94.5,
                0.25,
                0.0,
                OrderStatus::New,
            ),
            btc_exchange_order(
                "live-2",
                "unexpected-live-2",
                Side::Buy,
                94.6,
                0.25,
                0.0,
                OrderStatus::New,
            ),
        ],
        test_budget(),
        Duration::from_millis(50),
    )
    .await;

    let RuntimeHandles {
        market_task,
        user_task,
        effect_task,
        recovery_task,
        account_task,
    } = fixture.runtime.start().await.unwrap();
    market_task.abort();
    let _ = market_task.await;
    effect_task.abort();
    let _ = effect_task.await;

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    })
    .await;

    wait_until(|| fixture.exchange.canceled_order_ids.lock().unwrap().len() >= 2).await;
    assert_eq!(
        fixture
            .exchange
            .canceled_order_ids
            .lock()
            .unwrap()
            .as_slice(),
        ["live-1", "live-2"]
    );

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            .is_none()
    })
    .await;
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    recovery_task.abort();
    let _ = recovery_task.await;
    account_task.abort();
    let _ = account_task.await;
    user_task.abort();
    let _ = user_task.await;
}

#[tokio::test]
async fn recovery_task_still_cancels_unknown_live_orders_when_pending_submit_effect_exists() {
    let mut snapshot = test_snapshot();
    snapshot.desired_exposure = Some(Exposure(0.0));
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture_with_recovery_retry_interval(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![btc_exchange_order(
            "live-1",
            "unexpected-live",
            Side::Buy,
            94.5,
            0.25,
            0.0,
            OrderStatus::New,
        )],
        test_budget(),
        Duration::from_millis(200),
    )
    .await;

    let RuntimeHandles {
        market_task,
        user_task,
        effect_task,
        recovery_task,
        account_task,
    } = fixture.runtime.start().await.unwrap();
    market_task.abort();
    let _ = market_task.await;
    effect_task.abort();
    let _ = effect_task.await;

    wait_until_instance(&fixture.state, |instance| {
        instance
            .executor_state
            .diagnostics
            .recovery_anomaly
            .as_ref()
            == Some(&poise_engine::executor::RecoveryAnomaly::UnknownLiveOrder)
    })
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
                    price: 94.5,
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

    timeout(Duration::from_millis(800), async {
        wait_until(|| {
            !fixture
                .exchange
                .canceled_order_ids
                .lock()
                .unwrap()
                .is_empty()
        })
        .await;
    })
    .await
    .expect("unknown live order should still be auto-canceled with pending submit effect");
    assert_eq!(
        fixture
            .exchange
            .canceled_order_ids
            .lock()
            .unwrap()
            .as_slice(),
        ["live-1"]
    );

    timeout(Duration::from_millis(800), async {
        wait_until_instance(&fixture.state, |instance| {
            instance
                .executor_state
                .diagnostics
                .recovery_anomaly
                .as_ref()
                .is_none()
        })
        .await;
    })
    .await
    .expect("recovery anomaly should clear after auto-cancel");

    recovery_task.abort();
    let _ = recovery_task.await;
    account_task.abort();
    let _ = account_task.await;
    user_task.abort();
    let _ = user_task.await;
}

#[tokio::test]
async fn background_health_check_marks_market_data_stale_without_follow_up_events() {
    let started_at = Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap();
    let clock = Arc::new(MutableClock(Arc::new(Mutex::new(started_at))));
    let mut snapshot = test_snapshot();
    snapshot.status = TrackStatus::Paused;
    snapshot.desired_exposure = None;
    snapshot.executor_state = ExecutorState::empty(test_server_time());
    let fixture = runtime_fixture_with_options(
        Some(snapshot),
        btc_position(0.0, 0.0),
        vec![],
        test_budget(),
        RuntimeFixtureOptions {
            recovery_retry_interval: Duration::from_millis(50),
            audit_interval: Duration::from_secs(5),
            account_refresh_interval: Duration::from_secs(5),
            clock: clock.clone() as Arc<dyn ClockPort>,
        },
    )
    .await;

    let handles = fixture.runtime.start().await.unwrap();
    fixture.price_sender.send(btc_tick(95.0)).await.unwrap();

    wait_until_instance(&fixture.state, |instance| {
        instance.observed.last_tick_at.is_some()
    })
    .await;

    clock.set(Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 31).unwrap());

    wait_until_instance(&fixture.state, |instance| {
        instance.observed.market_data_stale_since.is_some()
    })
    .await;

    let instance = current_instance(&fixture.state).await;
    assert!(instance.observed.market_data_stale_since.is_some());
    assert!(fixture.exchange.submitted_orders.lock().unwrap().is_empty());

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_sync_replays_buffered_user_event_before_first_tick() {
    let fixture = runtime_fixture(None, btc_position(0.0, 0.0), vec![], test_budget()).await;
    fixture
        .user_sender
        .send(position_event_at(
            test_server_time() + chrono::Duration::milliseconds(1),
            7.5,
            5.0,
        ))
        .await
        .unwrap();

    let handles = fixture.runtime.start().await.unwrap();

    wait_until_instance(&fixture.state, |instance| {
        (instance.current_exposure.0 - 2.0).abs() < f64::EPSILON
    })
    .await;

    shutdown(handles).await;
}

#[tokio::test]
async fn startup_sync_ignores_buffered_user_event_older_than_cutoff() {
    let fixture = runtime_fixture(None, btc_position(7.5, 3.0), vec![], test_budget()).await;
    fixture
        .user_sender
        .send(position_event_at(
            test_server_time() - chrono::Duration::milliseconds(1),
            3.75,
            9.0,
        ))
        .await
        .unwrap();

    let handles = fixture.runtime.start().await.unwrap();

    let instance = current_instance(&fixture.state).await;
    assert_eq!(instance.current_exposure, Exposure(2.0));
    assert!((instance.risk.unrealized_pnl - 3.0).abs() < f64::EPSILON);

    shutdown(handles).await;
}

#[tokio::test]
async fn runtime_start_fails_when_buffered_user_data_replay_cannot_be_persisted() {
    let (price_sender, price_receiver) = mpsc::channel(8);
    drop(price_sender);
    let market_data = Arc::new(FakeMarketData::new(price_receiver));
    let exchange = Arc::new(FakeExchange::new(btc_position(0.0, 0.0), vec![]));
    let account = Arc::new(FakeAccountPort::with_user_events(vec![position_event_at(
        test_server_time() + chrono::Duration::milliseconds(1),
        7.5,
        5.0,
    )]));
    let persistence = Arc::new(FailOnSavePersistence::new(2));
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
        account_margin_guard.clone(),
    );
    let (state, worker_state) = build_runtime_and_effect_worker_test_contexts(
        &services,
        persistence.clone(),
        persistence.clone(),
        build_test_account_monitor(exchange.account_summary_port(), events).await,
        Arc::new(TrackProjector::new()),
    );
    let runtime = ServerRuntime::with_account_capacity_snapshots(
        state.runtime_state(),
        worker_state.effect_worker_state,
        RuntimePorts::new(
            exchange.execution_port(),
            market_data as Arc<dyn MarketDataPort>,
            account as Arc<dyn AccountPort>,
            exchange.metadata_port(),
        ),
        HashMap::new(),
        Duration::from_secs(1),
    );

    let error = runtime.start().await.err().unwrap();
    assert!(error.to_string().contains("injected save failure"));
}
