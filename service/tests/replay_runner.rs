use std::{fs, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use grid_platform_service::{
    execution::FakeExecutionAdapter,
    protocol::{CommandStatus, CommandType, OpenOrder, RuntimeSnapshot},
    replay::{ReplayAssertions, ReplayRunResult, ReplayScenario, ReplayStep, run_replay_scenario},
    storage::PersistedRuntime,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_runner_executes_json_fixture_and_records_market_fill_then_flatten() -> Result<()> {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay_buy_then_flatten.json");
    let scenario: ReplayScenario = serde_json::from_slice(
        &fs::read(&fixture_path)
            .with_context(|| format!("failed to read {}", fixture_path.display()))?,
    )
    .context("failed to decode replay fixture")?;

    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot = paused_snapshot_with_buy_order();

    let ReplayRunResult { snapshot, .. } =
        run_replay_scenario(runtime, Arc::new(FakeExecutionAdapter), &scenario).await?;

    assert!(snapshot.execution.open_orders.is_empty());
    assert_eq!(snapshot.execution.recent_fills.len(), 2);
    assert!(snapshot.execution.recent_fills.iter().any(|fill| {
        fill.client_order_id.as_deref() == Some("grid_buy_01") && fill.order_id == "ord_buy_01"
    }));
    assert!(snapshot.execution.recent_fills.iter().any(|fill| {
        fill.client_order_id.as_deref() == Some("reduce_only_cmd_flatten_replay")
            && fill.order_id == "order_cmd_flatten_replay"
    }));
    assert_eq!(snapshot.runtime.position_qty, 0.0);

    let ack = snapshot
        .execution
        .last_command_ack_event
        .as_ref()
        .context("expected flatten command ack")?;
    assert_eq!(ack.command, CommandType::FlattenNow);
    assert_eq!(ack.status, CommandStatus::Completed);
    assert_eq!(ack.command_id, "cmd_flatten_replay");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_runner_rejects_unexpected_command_status() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot = paused_snapshot_with_buy_order();

    let scenario = ReplayScenario {
        name: "status mismatch".into(),
        steps: vec![ReplayStep::Command {
            command: CommandType::FlattenNow,
            command_id: "cmd_expect_failed".into(),
            expect_status: Some(CommandStatus::Failed),
        }],
        assertions: ReplayAssertions::default(),
    };

    let error = run_replay_scenario(runtime, Arc::new(FakeExecutionAdapter), &scenario)
        .await
        .expect_err("expected replay status mismatch");
    assert!(error.to_string().contains("expected command status"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_runner_rejects_snapshot_assertion_mismatch() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot = paused_snapshot_with_buy_order();

    let scenario = ReplayScenario {
        name: "snapshot mismatch".into(),
        steps: vec![ReplayStep::Market {
            last_price: Some(99.5),
            mark_price: Some(99.5),
            emitted_at: "2025-01-01T00:00:01Z".into(),
        }],
        assertions: ReplayAssertions {
            position_qty: Some(0.0),
            ..ReplayAssertions::default()
        },
    };

    let error = run_replay_scenario(runtime, Arc::new(FakeExecutionAdapter), &scenario)
        .await
        .expect_err("expected replay assertion mismatch");
    assert!(error.to_string().contains("expected runtime.position_qty"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_runner_market_step_without_new_price_does_not_create_fill() -> Result<()> {
    let mut runtime = PersistedRuntime::in_memory_bootstrap();
    runtime.last_sequence = 1;
    runtime.snapshot = paused_snapshot_with_buy_order();

    let scenario = ReplayScenario {
        name: "heartbeat without price".into(),
        steps: vec![ReplayStep::Market {
            last_price: None,
            mark_price: None,
            emitted_at: "2025-01-01T00:00:01Z".into(),
        }],
        assertions: ReplayAssertions {
            open_order_count: Some(1),
            recent_fill_count: Some(0),
            ..ReplayAssertions::default()
        },
    };

    let ReplayRunResult { snapshot, .. } =
        run_replay_scenario(runtime, Arc::new(FakeExecutionAdapter), &scenario).await?;

    assert_eq!(snapshot.execution.open_orders.len(), 1);
    assert!(snapshot.execution.recent_fills.is_empty());

    Ok(())
}

fn paused_snapshot_with_buy_order() -> RuntimeSnapshot {
    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.strategy_state = "paused".into();
    snapshot.runtime.last_price = 100.0;
    snapshot.runtime.mark_price = 100.0;
    snapshot.runtime.position_qty = 0.0;
    snapshot.runtime.position_avg_price = 0.0;
    snapshot.runtime.unrealized_pnl = 0.0;
    snapshot.runtime.realized_pnl = 0.0;
    snapshot.execution.open_orders = vec![OpenOrder {
        order_id: "ord_buy_01".into(),
        client_order_id: "grid_buy_01".into(),
        side: "buy".into(),
        price: 100.0,
        qty: 1.0,
        filled_qty: 0.0,
        status: "NEW".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        updated_at: "2025-01-01T00:00:00Z".into(),
    }];
    snapshot.execution.recent_fills.clear();
    snapshot.execution.pending_commands.clear();
    snapshot.execution.last_command_ack = None;
    snapshot.execution.last_command_ack_event = None;
    snapshot.execution.recent_commands.clear();
    snapshot
}
