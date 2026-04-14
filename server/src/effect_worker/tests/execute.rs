use super::*;
use tokio::time::timeout;

#[tokio::test]
async fn submit_recovery_waits_while_recovery_anomaly_is_active() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    repository
        .seed_snapshot("btc-core", snapshot_with_recovery_anomaly())
        .await;
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
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;

    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        let snapshot = snapshot_with_recovery_anomaly();
        manager.restore_track_state(&snapshot).unwrap();
    }

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    timeout(Duration::from_secs(1), worker.run_once())
        .await
        .expect("submit recovery while anomaly is active should finish promptly")
        .unwrap();

    assert!(exchange.effects.lock().await.is_empty());
    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should remain pending");
    assert_eq!(effect.status, EffectStatus::Pending);
    assert!(
        !state.submit_preflight.take_pending_submit_effects_dirty(),
        "awaiting exchange state should not invalidate pending submit preflight tracking"
    );
}

#[tokio::test]
async fn effect_worker_does_not_dispatch_pending_auto_submit_when_price_gate_is_closed() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;
    let snapshot = snapshot_with_submit_pending_order(
        95.0,
        test_config(),
        WorkingOrder {
            order_id: None,
            client_order_id: "BTCUSDT-reconcile".into(),
            side: Side::Buy,
            price: 95.0,
            quantity: 15.0,
            status: OrderStatus::Submitting,
            role: poise_engine::executor::OrderRole::IncreaseInventory,
        },
    );

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
                    price: 95.0,
                    quantity: 15.0,
                    client_order_id: "BTCUSDT-reconcile".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(6.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
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
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    assert!(exchange.effects.lock().await.is_empty());
    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should remain persisted");
    assert_eq!(effect.status, EffectStatus::Superseded);
    assert!(
        state.submit_preflight.take_pending_submit_effects_dirty(),
        "superseded submit should invalidate pending submit preflight tracking"
    );
}

#[tokio::test]
async fn cancel_success_clears_working_order_slot_without_waiting_for_order_event() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;
    let snapshot = snapshot_with_working_order();

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
            effect: TrackEffect::CancelOrder {
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

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    let manager_handle = state.manager();
    let manager = manager_handle.read().await;
    let snapshot = manager.snapshot("btc-core").unwrap();
    assert_eq!(
        snapshot.executor_state.slots,
        vec![poise_engine::runtime::ExecutionSlot {
            slot: poise_engine::executor::OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }]
    );

    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("cancel effect should remain persisted");
    assert_eq!(effect.status, EffectStatus::Succeeded);
}

#[tokio::test]
async fn stale_cancel_effect_syncs_exchange_before_canceling() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;
    let snapshot = snapshot_with_working_order();

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
            effect: TrackEffect::CancelOrder {
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
    state
        .exchange_freshness
        .mark_stale("btc-core", ExchangeFreshnessReason::FilledAwaitingSync)
        .await;

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    assert_eq!(exchange.get_position_calls(), 1);
    assert_eq!(exchange.get_open_orders_calls(), 1);
    assert_eq!(
        repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("cancel effect should stay persisted")
            .status,
        EffectStatus::Pending
    );
}

#[tokio::test]
async fn stale_cancel_all_effect_syncs_exchange_before_canceling() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;
    let snapshot = snapshot_with_working_order();

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
            effect: TrackEffect::CancelAll {
                instrument: btc_instrument(),
            },
            status: EffectStatus::Pending,
            attempt_count: 0,
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .await;
    state
        .exchange_freshness
        .mark_stale("btc-core", ExchangeFreshnessReason::FilledAwaitingSync)
        .await;

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    assert_eq!(exchange.get_position_calls(), 1);
    assert_eq!(exchange.get_open_orders_calls(), 1);
    assert_eq!(
        repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("cancel-all effect should stay persisted")
            .status,
        EffectStatus::Pending
    );
}

#[tokio::test]
async fn cancel_unknown_order_sent_resyncs_exchange_state_before_marking_effect_failed() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::with_cancel_order_outcome_unknown(
        "request DELETE /fapi/v1/order failed with status 400 Bad Request: {\"code\":-2011,\"msg\":\"Unknown order sent.\"}",
    ));
    exchange.set_position_qty(15.0).await;
    let state = test_state(repository.clone()).await;
    let snapshot = snapshot_with_working_order();

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
            effect: TrackEffect::CancelOrder {
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

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("cancel effect should remain persisted");
    assert_eq!(effect.status, EffectStatus::Failed);
    assert_eq!(exchange.get_position_calls(), 1);
    assert_eq!(exchange.get_open_orders_calls(), 1);
}

#[tokio::test]
async fn fresh_effects_do_not_trigger_extra_sync() {
    let submit_repository = Arc::new(MemoryRepository::default());
    let submit_exchange = Arc::new(FakeExchange::default());
    let submit_state = test_state(submit_repository.clone()).await;
    submit_state.observe_market("btc-core", 95.0).await.unwrap();

    let submit_worker = EffectWorker::new(
        submit_state,
        submit_exchange.execution_port(),
        submit_exchange.account_port(),
        Duration::from_secs(60),
    );
    submit_worker.run_once().await.unwrap();

    assert_eq!(submit_exchange.get_position_calls(), 0);
    assert_eq!(submit_exchange.get_open_orders_calls(), 0);

    let cancel_repository = Arc::new(MemoryRepository::default());
    let cancel_exchange = Arc::new(FakeExchange::default());
    let cancel_state = test_state(cancel_repository.clone()).await;
    let snapshot = snapshot_with_working_order();
    cancel_repository
        .seed_snapshot("btc-core", snapshot.clone())
        .await;
    {
        let manager_handle = cancel_state.manager();
        let mut manager = manager_handle.write().await;
        manager.restore_track_state(&snapshot).unwrap();
    }
    cancel_repository
        .seed_effect(PersistedTrackEffect {
            effect_id: "btc-core:cancel:0".into(),
            track_id: TrackId::new("btc-core"),
            batch_id: "cancel".into(),
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
        })
        .await;

    let cancel_worker = EffectWorker::new(
        cancel_state,
        cancel_exchange.execution_port(),
        cancel_exchange.account_port(),
        Duration::from_secs(60),
    );
    cancel_worker.run_once().await.unwrap();

    assert_eq!(cancel_exchange.get_position_calls(), 0);
    assert_eq!(cancel_exchange.get_open_orders_calls(), 0);
}
