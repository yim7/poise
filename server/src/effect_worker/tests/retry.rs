use super::*;

#[tokio::test]
async fn cancel_unknown_order_sent_retires_follow_up_after_terminal_update_arrives() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::with_cancel_order_error(
        "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
    ));
    exchange.set_position_qty(15.0).await;
    exchange.open_orders.lock().await.push(ExchangeOrder {
        instrument: btc_instrument(),
        order_id: "order-1".into(),
        client_order_id: "client-1".into(),
        side: Side::Buy,
        price: 95.0,
        qty: 15.0,
        realized_pnl: 0.0,
        status: OrderStatus::New,
    });
    let state = test_state(repository.clone(), exchange.clone()).await;
    let snapshot = snapshot_with_working_order();

    repository.seed_snapshot("btc-core", snapshot.clone()).await;
    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        manager.restore_track_state(&snapshot).unwrap();
    }
    let cancel_effect = PersistedTrackEffect {
        effect_id: "btc-core:batch:0".into(),
        track_id: TrackId::new("btc-core"),
        batch_id: "batch".into(),
        sequence: 0,
        effect: TrackEffect::CancelOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
        },
        status: EffectStatus::Pending,
        attempt_count: 0,
        last_error: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    repository.seed_effect(cancel_effect.clone()).await;
    repository
        .seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:batch:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 1,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    client_order_id: "btc-core-replacement".into(),
                    side: Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let worker = EffectWorker::new(
        state.clone(),
        exchange.clone() as Arc<dyn ExchangePort>,
        Duration::from_secs(60),
    );
    let error = worker
        .execute_cancellation(
            &cancel_effect,
            Cancellation::One {
                instrument: btc_instrument(),
                order_id: "order-1".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Unknown order sent."));
    assert_eq!(
        repository
            .list_all_effects()
            .await
            .iter()
            .find(|effect| effect.effect_id == "btc-core:batch:1")
            .map(|effect| effect.status),
        Some(EffectStatus::Pending)
    );

    state
        .observe_order_with_absorb_result(
            "btc-core",
            OrderObservation {
                order_id: "order-1".into(),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 95.0,
                quantity: 15.0,
                realized_pnl: 0.0,
                status: OrderStatus::Filled,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        repository
            .list_all_effects()
            .await
            .iter()
            .find(|effect| effect.effect_id == "btc-core:batch:1")
            .map(|effect| effect.status),
        Some(EffectStatus::Superseded)
    );
}

#[tokio::test]
async fn cancel_unknown_order_sent_still_marks_cancel_effect_failed_when_follow_up_retry_errors() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::with_cancel_order_error(
        "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
    ));
    exchange.set_position_qty(15.0).await;
    exchange.open_orders.lock().await.push(ExchangeOrder {
        instrument: btc_instrument(),
        order_id: "order-1".into(),
        client_order_id: "client-1".into(),
        side: Side::Buy,
        price: 95.0,
        qty: 15.0,
        realized_pnl: 0.0,
        status: OrderStatus::New,
    });
    let state = test_state(repository.clone(), exchange.clone()).await;
    let snapshot = snapshot_with_working_order();

    repository.seed_snapshot("btc-core", snapshot.clone()).await;
    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        manager.restore_track_state(&snapshot).unwrap();
    }
    let cancel_effect = PersistedTrackEffect {
        effect_id: "btc-core:broken:0".into(),
        track_id: TrackId::new("btc-core"),
        batch_id: "broken".into(),
        sequence: 0,
        effect: TrackEffect::CancelOrder {
            instrument: btc_instrument(),
            order_id: "order-1".into(),
        },
        status: EffectStatus::Pending,
        attempt_count: 0,
        last_error: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    repository.seed_effect(cancel_effect.clone()).await;
    repository
        .seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:broken:1".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "broken".into(),
            sequence: 1,
            effect: TrackEffect::CancelOrder {
                instrument: btc_instrument(),
                order_id: "unexpected-cancel".into(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let worker = EffectWorker::new(
        state.clone(),
        exchange.clone() as Arc<dyn ExchangePort>,
        Duration::from_secs(60),
    );
    let error = worker
        .execute_cancellation(
            &cancel_effect,
            Cancellation::One {
                instrument: btc_instrument(),
                order_id: "order-1".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Unknown order sent."));
    assert_eq!(
        repository
            .list_all_effects()
            .await
            .iter()
            .find(|effect| effect.effect_id == "btc-core:broken:0")
            .map(|effect| effect.status),
        Some(EffectStatus::Failed)
    );
    assert!(
        repository
            .list_all_effects()
            .await
            .iter()
            .find(|effect| effect.effect_id == "btc-core:broken:0")
            .and_then(|effect| effect.last_error.as_deref())
            .is_some_and(|error| error.contains("Unknown order sent."))
    );
}

#[tokio::test]
async fn submit_recovery_proceed_keeps_active_pending_target_when_rounded_request_matches() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let config = TrackConfig {
        lower_price: 90.0,
        upper_price: 110.0,
        long_exposure_units: 8.0,
        short_exposure_units: 8.0,
        notional_per_unit: 100.0,
        min_rebalance_units: 0.5,
        shape_family: ShapeFamily::Linear,
        out_of_band_policy: OutOfBandPolicy::Freeze,
    };
    let exchange_rules = ExchangeRules {
        price_tick: 10.0,
        quantity_step: 1.0,
        min_qty: 0.0,
        min_notional: 0.0,
        maker_fee_rate: 0.0,
        taker_fee_rate: 0.0,
    };
    let state = test_state_with_track(
        repository.clone(),
        exchange.clone(),
        config.clone(),
        exchange_rules,
    )
    .await;
    let snapshot = snapshot_with_submit_pending_order(
        94.99,
        config.clone(),
        WorkingOrder {
            order_id: None,
            client_order_id: "btc-core-reconcile".into(),
            side: Side::Buy,
            price: 90.0,
            quantity: 4.0,
            status: OrderStatus::Submitting,
            role: poise_engine::executor::OrderRole::IncreaseInventory,
        },
    );
    let expected_round_target = snapshot
        .desired_exposure
        .clone()
        .expect("snapshot should carry desired exposure");

    repository.seed_snapshot("btc-core", snapshot.clone()).await;
    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        manager.restore_track_state(&snapshot).unwrap();
    }
    repository
        .seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:batch:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "batch".into(),
            sequence: 0,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 90.0,
                    quantity: 4.0,
                    client_order_id: "btc-core-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let worker = EffectWorker::new(
        state.clone(),
        exchange as Arc<dyn ExchangePort>,
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    let manager_handle = state.manager();
    let manager = manager_handle.read().await;
    let snapshot = manager.snapshot("btc-core").unwrap();
    assert_eq!(
        snapshot
            .executor_state
            .active_round
            .as_ref()
            .map(|round| round.desired_exposure.clone()),
        Some(expected_round_target)
    );
}

#[tokio::test]
async fn effect_worker_stops_polling_new_effects_after_shutdown_signal() {
    let repository = Arc::new(MemoryRepository::default());
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        submit_started.clone(),
        release_submit.clone(),
    ));
    let state = test_state(repository.clone(), exchange.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    let submit_effect = match transition.effects.as_slice() {
        [TrackEffect::SubmitOrder { .. }] => repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should be persisted"),
        other => panic!("expected one submit effect, got {other:?}"),
    };
    repository
        .seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:shutdown:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "shutdown".into(),
            sequence: 0,
            effect: TrackEffect::NoOp,
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker = EffectWorker::with_shutdown_rx(
        state.clone(),
        exchange.clone() as Arc<dyn ExchangePort>,
        Duration::from_millis(1),
        shutdown_rx,
    );
    let task = worker.spawn();

    submit_started.notified().await;
    shutdown_tx.send(true).unwrap();
    release_submit.notify_waiters();

    timeout(Duration::from_secs(1), async {
        task.await.unwrap();
    })
    .await
    .unwrap();

    let effects = repository.list_all_effects().await;
    let submit = effects
        .iter()
        .find(|effect| effect.effect_id == submit_effect.effect_id)
        .expect("submit effect should still exist");
    let no_op = effects
        .iter()
        .find(|effect| effect.effect_id == "btc-core:shutdown:0")
        .expect("no-op effect should still exist");

    assert_eq!(exchange.effects.lock().await.len(), 1);
    assert_eq!(submit.status, EffectStatus::Succeeded);
    assert_eq!(no_op.status, EffectStatus::Pending);
}

#[tokio::test]
async fn submit_receipt_unmatched_resyncs_exchange_state_before_marking_effect_failed() {
    let repository = Arc::new(MemoryRepository::default());
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        submit_started.clone(),
        release_submit.clone(),
    ));
    exchange.set_position_qty(15.0).await;
    let state = test_state(repository.clone(), exchange.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));

    let worker = EffectWorker::new(
        state.clone(),
        exchange.clone() as Arc<dyn ExchangePort>,
        Duration::from_secs(60),
    );
    let task = tokio::spawn(async move { worker.run_once().await });

    submit_started.notified().await;

    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        let mut snapshot = manager.snapshot("btc-core").unwrap();
        snapshot.executor_state.slots = vec![poise_engine::runtime::ExecutionSlot {
            slot: poise_engine::executor::OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }];
        manager.restore_track_state(&snapshot).unwrap();
    }

    release_submit.notify_waiters();
    task.await.unwrap().unwrap();

    let manager_handle = state.manager();
    let manager = manager_handle.read().await;
    let snapshot = manager.snapshot("btc-core").unwrap();
    assert_eq!(snapshot.current_exposure, Exposure(4.0));
    assert_eq!(snapshot.desired_exposure, Some(Exposure(4.0)));
    assert!(
        snapshot
            .executor_state
            .diagnostics
            .recovery_anomaly
            .is_none()
    );

    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should remain persisted");
    assert_eq!(effect.status, EffectStatus::Failed);
    assert!(
        effect
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("submit receipt did not match executor slot"))
    );
    assert_eq!(exchange.get_position_calls(), 1);
    assert_eq!(exchange.get_open_orders_calls(), 1);
}

#[tokio::test]
async fn outcome_unknown_marks_track_stale_before_reconcile() {
    let repository = Arc::new(MemoryRepository::default());
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let get_position_started = Arc::new(Notify::new());
    let release_get_position = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit_and_get_position(
        submit_started.clone(),
        release_submit.clone(),
        get_position_started.clone(),
        release_get_position.clone(),
    ));
    exchange.set_position_qty(15.0).await;
    let state = test_state(repository.clone(), exchange.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));

    let worker = EffectWorker::new(
        state.clone(),
        exchange.clone() as Arc<dyn ExchangePort>,
        Duration::from_secs(60),
    );
    let task = tokio::spawn(async move { worker.run_once().await });

    submit_started.notified().await;

    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        let mut snapshot = manager.snapshot("btc-core").unwrap();
        snapshot.executor_state.slots = vec![poise_engine::runtime::ExecutionSlot {
            slot: poise_engine::executor::OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }];
        manager.restore_track_state(&snapshot).unwrap();
    }

    release_submit.notify_waiters();
    get_position_started.notified().await;
    assert!(state.exchange_freshness.is_stale("btc-core").await);
    release_get_position.notify_waiters();
    task.await.unwrap().unwrap();

    assert!(!state.exchange_freshness.is_stale("btc-core").await);
}
