use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use grid_platform_service::{
    execution::{ExecutionAdapter, ExecutionOutcome, ScriptedExecutionAdapter},
    kernel::{EngineEvent, spawn_engine, spawn_engine_with_adapter},
    protocol::{CommandRequest, CommandStatus, CommandType, RiskLevel, RuntimeSnapshot, StrategyStatus},
    storage::PersistedRuntime,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

struct BlockingExecutionAdapter {
    ready: Arc<Notify>,
}

#[async_trait]
impl ExecutionAdapter for BlockingExecutionAdapter {
    async fn execute(
        &self,
        _command: CommandType,
        _command_id: &str,
        _snapshot: &RuntimeSnapshot,
    ) -> anyhow::Result<ExecutionOutcome> {
        self.ready.notified().await;
        Ok(ExecutionOutcome::completed("All open orders cancelled."))
    }
}

fn spawn_submit_command(
    engine: grid_platform_service::kernel::EngineHandle,
    command_id: &str,
) -> JoinHandle<anyhow::Result<grid_platform_service::protocol::CommandAccepted>> {
    let request = CommandRequest {
        command_id: command_id.into(),
    };
    tokio::spawn(async move { engine.submit_command(CommandType::CancelAll, request).await })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_flow_updates_read_model_and_publishes_ack() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
    assert_eq!(
        read_model.read().expect("read model").system_events()[0].message,
        "Rust in-memory runtime bootstrapped."
    );

    let accepted = engine
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_kernel".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);
    assert_eq!(
        read_model
            .read()
            .expect("read model")
            .snapshot()
            .runtime
            .strategy_state,
        "paused"
    );
    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_kernel")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].command_id,
        "cmd_pause_kernel"
    );

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    assert_eq!(event.sequence, 1);
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_pause_kernel");
            assert_eq!(ack.command, CommandType::Pause);
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    assert_eq!(
        read_model.read().expect("read model").system_events()[0].source,
        "commands"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_updates_read_model_and_publishes_event() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
    let initial = read_model
        .read()
        .expect("read model")
        .snapshot()
        .runtime
        .last_price;

    let tick = engine.emit_price_tick().await?;
    assert!(tick.last_price > initial);

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.last_price, tick.last_price);
    assert_eq!(snapshot.runtime.mark_price, tick.mark_price);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    assert_eq!(event.sequence, 1);
    match event.event {
        EngineEvent::PriceUpdated(event) => assert_eq!(event.last_price, tick.last_price),
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_command_returns_accepted_before_async_completion() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let submit = spawn_submit_command(engine.clone(), "cmd_async_cancel");

    let accepted = timeout(Duration::from_millis(100), submit).await???;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .any(|item| item.command_id == "cmd_async_cancel")
    );
    assert!(snapshot.execution.last_command_ack_event.is_none());

    ready.notify_waiters();

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_async_cancel");
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_command_times_out_on_service_when_adapter_stalls() -> Result<()> {
    let adapter = BlockingExecutionAdapter {
        ready: Arc::new(Notify::new()),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_service".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_timeout_service");
            assert_eq!(ack.status, CommandStatus::TimedOut);
            assert!(ack.message.contains("timed out"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .all(|item| item.command_id != "cmd_timeout_service")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_execution_result_does_not_override_timed_out_terminal_state() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_then_late_result".into(),
            },
        )
        .await?;
    assert_eq!(accepted.status, CommandStatus::Accepted);

    let timed_out = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack)
                    if ack.command_id == "cmd_timeout_then_late_result" =>
                {
                    break ack;
                }
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(timed_out.status, CommandStatus::TimedOut);

    ready.notify_waiters();

    assert!(
        timeout(Duration::from_millis(300), async {
            loop {
                let event = events_rx.recv().await.expect("engine event");
                match event.event {
                    EngineEvent::CommandAck(ack)
                        if ack.command_id == "cmd_timeout_then_late_result" =>
                    {
                        break ack;
                    }
                    _ => continue,
                }
            }
        })
        .await
        .is_err()
    );

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].summary,
        "Execution timed out while waiting for terminal result."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_marks_strategy_pending_rebuild_when_inventory_blocks_rebuild() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.strategy.config.rebuild_threshold_bps = 0.1;
    runtime.snapshot.strategy.rebuild_reference_price = runtime.snapshot.runtime.last_price;

    let (engine, read_model, _events_rx) = grid_platform_service::kernel::spawn_engine_with_runtime(runtime, None);

    engine.emit_price_tick().await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.strategy.status, StrategyStatus::PendingRebuild);
    assert!(snapshot.strategy.pending_rebuild_reason.is_some());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn price_tick_engages_breaker_and_broadcasts_risk_alert_when_stop_loss_is_triggered(
) -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.snapshot = RuntimeSnapshot::sample();
    runtime.snapshot.runtime.last_price = 100.0;
    runtime.snapshot.runtime.mark_price = 100.0;
    runtime.snapshot.runtime.position_qty = -0.25;
    runtime.snapshot.runtime.position_avg_price = 100.0;
    runtime.snapshot.risk.stop_loss_pct = 0.05;

    let (engine, read_model, mut events_rx) =
        grid_platform_service::kernel::spawn_engine_with_runtime(runtime, None);

    engine.emit_price_tick().await?;

    let risk_event = timeout(Duration::from_secs(1), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::RiskAlert(alert) => break alert,
                _ => continue,
            }
        }
    })
    .await?;

    assert_eq!(risk_event.code, "STOP_LOSS_TRIGGERED");

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(snapshot.risk.breaker_engaged);
    assert_eq!(snapshot.risk.risk_level, RiskLevel::Danger);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_execution_command_fails_while_another_is_in_flight() -> Result<()> {
    let ready = Arc::new(Notify::new());
    let adapter = BlockingExecutionAdapter {
        ready: ready.clone(),
    };
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    let first = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_in_flight_01".into(),
            },
        )
        .await?;
    assert_eq!(first.status, CommandStatus::Accepted);

    let second = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_in_flight_02".into(),
            },
        )
        .await?;
    assert_eq!(second.status, CommandStatus::Accepted);

    let rejected = timeout(Duration::from_millis(100), async {
        loop {
            let event = events_rx.recv().await.expect("engine event");
            match event.event {
                EngineEvent::CommandAck(ack) if ack.command_id == "cmd_in_flight_02" => break ack,
                _ => continue,
            }
        }
    })
    .await?;
    assert_eq!(rejected.status, CommandStatus::Failed);
    assert!(rejected.message.contains("in flight"));

    let snapshot = read_model.read().expect("read model").snapshot();
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .any(|item| item.command_id == "cmd_in_flight_01")
    );
    assert!(
        snapshot
            .execution
            .pending_commands
            .iter()
            .all(|item| item.command_id != "cmd_in_flight_02")
    );

    ready.notify_waiters();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_command_records_reason_without_side_effects() -> Result<()> {
    let adapter = ScriptedExecutionAdapter::new();
    adapter.push_outcome(
        "cmd_fail_cancel",
        ExecutionOutcome::failed("exchange rejected cancel-all"),
    );
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));
    let before = read_model.read().expect("read model").snapshot();
    let open_orders_before = before.execution.open_orders.len();
    let fills_before = before.execution.recent_fills.len();

    let accepted = engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_fail_cancel".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_fail_cancel");
            assert_eq!(ack.status, CommandStatus::Failed);
            assert_eq!(ack.message, "exchange rejected cancel-all");
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.open_orders.len(), open_orders_before);
    assert_eq!(snapshot.execution.recent_fills.len(), fills_before);
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::Failed
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].summary,
        "exchange rejected cancel-all"
    );
    assert_eq!(
        snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .expect("ack event")
            .status,
        CommandStatus::Failed
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_command_records_reason_without_side_effects() -> Result<()> {
    let adapter = ScriptedExecutionAdapter::new();
    adapter.push_outcome(
        "cmd_timeout_flatten",
        ExecutionOutcome::timed_out("flatten timed out"),
    );
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));
    let before = read_model.read().expect("read model").snapshot();
    let position_before = before.runtime.position_qty;
    let fills_before = before.execution.recent_fills.len();

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_timeout_flatten".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_timeout_flatten");
            assert_eq!(ack.status, CommandStatus::TimedOut);
            assert_eq!(ack.message, "flatten timed out");
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.position_qty, position_before);
    assert_eq!(snapshot.execution.recent_fills.len(), fills_before);
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::TimedOut
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].summary,
        "flatten timed out"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idempotent_command_keeps_single_record_and_reason() -> Result<()> {
    let adapter = ScriptedExecutionAdapter::new();
    adapter.push_outcome(
        "cmd_idempotent_cancel",
        ExecutionOutcome::completed("All open orders cancelled."),
    );
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_idempotent_cancel".into(),
            },
        )
        .await?;
    let _ = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");

    let first_snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(first_snapshot.execution.recent_commands.len(), 1);

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_idempotent_cancel".into(),
            },
        )
        .await?;

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.execution.recent_commands.len(), 1);
    assert!(
        snapshot.execution.recent_commands[0]
            .summary
            .contains("Idempotent hit")
    );
    assert_eq!(
        snapshot.execution.recent_commands[0].status,
        CommandStatus::Completed
    );

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_idempotent_cancel");
            assert_eq!(ack.status, CommandStatus::Completed);
            assert!(ack.message.contains("Idempotent hit"));
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reusing_command_id_with_different_command_type_is_rejected() -> Result<()> {
    let adapter = ScriptedExecutionAdapter::new();
    adapter.push_outcome(
        "cmd_mismatched_id",
        ExecutionOutcome::completed("All open orders cancelled."),
    );
    let (engine, read_model, mut events_rx) = spawn_engine_with_adapter(Arc::new(adapter));

    engine
        .submit_command(
            CommandType::CancelAll,
            CommandRequest {
                command_id: "cmd_mismatched_id".into(),
            },
        )
        .await?;
    let _ = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");

    let before = read_model.read().expect("read model").snapshot();
    let error = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_mismatched_id".into(),
            },
        )
        .await
        .expect_err("mismatched command_id reuse should be rejected");
    assert!(error.to_string().contains("different command"));

    let after = read_model.read().expect("read model").snapshot();
    assert_eq!(after.execution.recent_commands.len(), 1);
    assert_eq!(
        after.execution.recent_commands[0].command,
        CommandType::CancelAll
    );
    assert_eq!(
        after.execution.last_command_ack_event,
        before.execution.last_command_ack_event
    );
    assert!(
        timeout(Duration::from_millis(200), events_rx.recv())
            .await
            .is_err()
    );

    Ok(())
}
