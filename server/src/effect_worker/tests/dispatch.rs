use super::*;

#[tokio::test]
async fn effect_worker_accepts_distinct_execution_and_account_ports() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository).await;

    let worker = EffectWorker::new(
        state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();
}

#[tokio::test]
async fn submit_success_updates_working_order_via_receipt_writeback() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));

    {
        let manager_handle = state.manager();
        let mut manager = manager_handle.write().await;
        let mut snapshot = manager.snapshot("btc-core").unwrap();
        snapshot
            .executor_state
            .slots
            .push(poise_engine::runtime::ExecutionSlot {
                slot: poise_engine::executor::OrderSlot::new("inventory_followup"),
                state: SlotState::Empty,
                working_order: None,
            });
        manager.restore_track_state(&snapshot).unwrap();
        repository.seed_snapshot("btc-core", snapshot).await;
    }
    let replay = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(
        replay.effects.is_empty(),
        "restored pending submit should only rehydrate live quote before receipt writeback"
    );

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
    let slot = snapshot
        .executor_state
        .slots
        .first()
        .expect("submit receipt should update working order slot");
    assert_eq!(slot.state, SlotState::Working);
    let order = slot
        .working_order
        .as_ref()
        .expect("slot should keep working order after receipt");
    assert_eq!(order.order_id.as_deref(), Some("order-1"));
    assert_eq!(order.status, OrderStatus::New);
    assert_eq!(snapshot.executor_state.slots.len(), 2);
    assert_eq!(
        snapshot.executor_state.slots[1].slot,
        poise_engine::executor::OrderSlot::new("inventory_followup")
    );
    assert_eq!(snapshot.executor_state.slots[1].state, SlotState::Empty);

    let effect = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should remain persisted");
    assert_eq!(effect.status, EffectStatus::Succeeded);
}

#[tokio::test]
async fn effect_worker_writeback_keeps_round_target_without_working_order_target_copy() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));
    let expected_round_target = match transition.effects.as_slice() {
        [
            TrackEffect::SubmitOrder {
                desired_exposure, ..
            },
        ] => desired_exposure.clone(),
        _ => panic!("expected a single submit effect"),
    };

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
    let executor = serde_json::to_value(&snapshot).unwrap()["executor_state"]
        .as_object()
        .expect("executor state should serialize as an object")
        .clone();
    let active_round = executor
        .get("active_round")
        .and_then(|value| value.as_object())
        .expect("receipt writeback should preserve active_round");
    let working_order = executor["slots"][0]["working_order"]
        .as_object()
        .expect("working order should be present after receipt");

    assert_eq!(
        active_round["desired_exposure"],
        serde_json::json!(expected_round_target.0)
    );
    assert!(
        !working_order.contains_key("desired_exposure"),
        "working order should not keep a target copy after writeback"
    );
}

#[tokio::test]
async fn fresh_submit_uses_direct_preflight_without_open_orders_lookup() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));

    let worker = EffectWorker::new(
        state,
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    assert_eq!(exchange.get_open_orders_calls(), 0);
}

#[tokio::test]
async fn submit_coordinator_marks_submit_started_before_returning_flight() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    let (request, desired_exposure) = match transition.effects.as_slice() {
        [
            TrackEffect::SubmitOrder {
                request,
                desired_exposure,
                ..
            },
        ] => (request.clone(), desired_exposure.clone()),
        _ => panic!("expected a single submit effect"),
    };
    let persisted = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should be persisted");

    let coordinator = crate::submit_coordinator::SubmitCoordinator::new(
        exchange.execution_port(),
        state.effect_worker_state.submit_effect_service.clone(),
        state.submit_preflight.clone(),
    );

    let flight = coordinator
        .prepare(&persisted, request.clone(), desired_exposure)
        .await
        .unwrap()
        .expect("fresh submit should produce a flight");
    let (prepared_request, completion) = flight.into_parts();

    assert!(
        state
            .submit_preflight
            .is_attempted(&persisted.effect_id)
            .await,
        "flight should already mark the submit as started before returning"
    );
    assert_eq!(prepared_request, request);
    completion.record_failure("submit rejected").await.unwrap();
}

#[tokio::test]
async fn stale_submit_effect_syncs_exchange_before_submitting() {
    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));
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

    assert!(exchange.effects.lock().await.is_empty());
    assert_eq!(exchange.get_position_calls(), 1);
    assert_eq!(exchange.get_open_orders_calls(), 1);
    assert_eq!(
        repository
            .list_all_effects()
            .await
            .into_iter()
            .next()
            .expect("submit effect should stay persisted")
            .status,
        EffectStatus::Pending
    );
}

#[tokio::test]
async fn mark_submit_started_happens_only_after_prepare_returns_some() {
    let repository = Arc::new(MemoryRepository::default());
    let submit_started = Arc::new(Notify::new());
    let release_submit = Arc::new(Notify::new());
    let exchange = Arc::new(FakeExchange::with_blocked_submit(
        submit_started.clone(),
        release_submit.clone(),
    ));
    let state = test_state(repository.clone()).await;

    let transition = state.observe_market("btc-core", 95.0).await.unwrap();
    assert!(matches!(
        transition.effects.as_slice(),
        [TrackEffect::SubmitOrder { .. }]
    ));
    let effect_id = repository
        .list_all_effects()
        .await
        .into_iter()
        .next()
        .expect("submit effect should be persisted")
        .effect_id;

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    let task = tokio::spawn(async move { worker.run_once().await });
    submit_started.notified().await;
    let attempted_after_prepare = state.submit_preflight.is_attempted(&effect_id).await;
    release_submit.notify_waiters();
    task.await.unwrap().unwrap();

    assert!(attempted_after_prepare);

    let repository = Arc::new(MemoryRepository::default());
    let exchange = Arc::new(FakeExchange::default());
    let state = test_state(repository.clone()).await;

    repository
        .seed_snapshot("btc-core", snapshot_with_recovery_anomaly())
        .await;
    let skipped_effect_id = "btc-core:skip:0".to_string();
    repository
        .seed_effect(PersistedTrackEffect {
            effect_id: skipped_effect_id.clone(),
            track_id: TrackId::new("btc-core"),
            batch_id: "skip".into(),
            sequence: 0,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: btc_instrument(),
                    side: Side::Buy,
                    price: 94.0,
                    quantity: 0.25,
                    client_order_id: "BTCUSDT-skip".into(),
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
        manager
            .restore_track_state(&snapshot_with_recovery_anomaly())
            .unwrap();
    }

    let worker = EffectWorker::new(
        state.clone(),
        exchange.execution_port(),
        exchange.account_port(),
        Duration::from_secs(60),
    );
    worker.run_once().await.unwrap();

    assert!(
        !state
            .submit_preflight
            .is_attempted(&skipped_effect_id)
            .await
    );
}

#[tokio::test]
async fn submit_preflight_assumes_single_effect_worker_execution_order() {
    let preflight = SubmitPreflight::new();
    preflight.mark_submit_started("effect-1").await;

    let started_decision = preflight.decide("effect-1", "client-1").await;
    let fresh_decision = preflight.decide("effect-2", "client-2").await;

    assert_eq!(
        started_decision,
        SubmitPreflightDecision::NeedsLiveOrderLookup {
            client_order_id: "client-1".into()
        }
    );
    assert_eq!(fresh_decision, SubmitPreflightDecision::Direct);
}
