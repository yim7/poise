use std::time::Duration;

#[path = "support/runtime_seed.rs"]
mod runtime_seed;

use anyhow::Result;
use grid_platform_service::{
    execution::{CancelOrdersRequest, ExecutionAdapter, FakeExecutionAdapter, SubmitOrderRequest},
    kernel::{EngineEvent, spawn_engine_with_runtime},
    protocol::{CommandRequest, CommandStatus, CommandType, RuntimeSnapshot},
};
use tokio::time::timeout;

#[tokio::test]
async fn fake_adapter_cancel_orders_filters_matching_open_orders() -> Result<()> {
    let adapter = FakeExecutionAdapter;
    let snapshot = RuntimeSnapshot::sample();

    let open_orders = adapter
        .cancel_orders(
            CancelOrdersRequest {
                command_id: Some("cmd_cancel_filter".into()),
                order_ids: vec!["ord_1002".into()],
                client_order_ids: vec!["grid_buy_01".into()],
            },
            &snapshot,
        )
        .await?;

    assert!(open_orders.is_empty());

    Ok(())
}

#[tokio::test]
async fn fake_adapter_submit_grid_order_returns_open_order_fact() -> Result<()> {
    let adapter = FakeExecutionAdapter;
    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.execution.open_orders.clear();

    let submitted = adapter
        .submit_order(
            SubmitOrderRequest {
                command_id: None,
                order_id: "ord_grid_buy_02".into(),
                client_order_id: "grid_buy_02".into(),
                side: "buy".into(),
                price: 2344.95,
                qty: 0.1,
                reduce_only: false,
            },
            &snapshot,
        )
        .await?;

    let open_order = submitted.open_order.expect("grid order fact");
    snapshot.execution.open_orders.push(open_order.clone());

    let open_orders = adapter.query_open_orders(&snapshot).await?;
    assert_eq!(open_orders, vec![open_order]);

    Ok(())
}

#[tokio::test]
async fn fake_adapter_submit_reduce_only_order_returns_fill_fact() -> Result<()> {
    let adapter = FakeExecutionAdapter;
    let mut snapshot = RuntimeSnapshot::sample();

    let submitted = adapter
        .submit_order(
            SubmitOrderRequest {
                command_id: Some("cmd_reduce_only_fill".into()),
                order_id: "order_cmd_reduce_only_fill".into(),
                client_order_id: "reduce_only_cmd_reduce_only_fill".into(),
                side: "sell".into(),
                price: snapshot.runtime.mark_price,
                qty: snapshot.runtime.position_qty,
                reduce_only: true,
            },
            &snapshot,
        )
        .await?;

    let fill = submitted.fill.expect("reduce-only fill fact");
    snapshot.execution.recent_fills.insert(0, fill.clone());

    let fills = adapter.list_recent_fills(&snapshot).await?;
    assert_eq!(fills[0], fill);

    Ok(())
}

#[tokio::test]
async fn fake_adapter_submit_reduce_only_order_clips_fill_qty_to_position() -> Result<()> {
    let adapter = FakeExecutionAdapter;
    let snapshot = RuntimeSnapshot::sample();

    let submitted = adapter
        .submit_order(
            SubmitOrderRequest {
                command_id: Some("cmd_reduce_only_clip".into()),
                order_id: "order_cmd_reduce_only_clip".into(),
                client_order_id: "reduce_only_cmd_reduce_only_clip".into(),
                side: "sell".into(),
                price: snapshot.runtime.mark_price,
                qty: snapshot.runtime.position_qty * 2.0,
                reduce_only: true,
            },
            &snapshot,
        )
        .await?;

    let fill = submitted.fill.expect("reduce-only fill fact");
    assert_eq!(fill.qty, snapshot.runtime.position_qty);
    assert_close(
        fill.realized_pnl,
        (snapshot.runtime.mark_price - snapshot.runtime.position_avg_price)
            * snapshot.runtime.position_qty,
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flatten_now_records_reduce_only_fill_in_execution_facts() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_runtime(runtime_seed::seed_runtime_with_position_and_orders(), None);
    let before = read_model.read().expect("read model").snapshot();
    let previous_fill_count = before.execution.recent_fills.len();
    let position_qty = before.runtime.position_qty;
    let mark_price = before.runtime.mark_price;

    let accepted = engine
        .submit_command(
            CommandType::FlattenNow,
            CommandRequest {
                command_id: "cmd_flatten_fill".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_flatten_fill");
            assert_eq!(ack.command, CommandType::FlattenNow);
            assert_eq!(ack.status, CommandStatus::Completed);
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.position_qty, 0.0);
    assert_close(
        snapshot.runtime.realized_pnl,
        before.runtime.realized_pnl
            + (mark_price - before.runtime.position_avg_price) * position_qty,
    );
    assert_eq!(
        snapshot.execution.recent_fills.len(),
        previous_fill_count + 1
    );

    let fill = snapshot
        .execution
        .recent_fills
        .first()
        .expect("flatten fill fact");
    assert_eq!(fill.side, "sell");
    assert_eq!(fill.qty, position_qty);
    assert_eq!(fill.price, mark_price);
    assert_close(
        fill.realized_pnl,
        (mark_price - before.runtime.position_avg_price) * position_qty,
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_after_flatten_pauses_strategy_and_records_fill_fact() -> Result<()> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_runtime(runtime_seed::seed_runtime_with_position_and_orders(), None);
    let before = read_model.read().expect("read model").snapshot();
    let previous_fill_count = before.execution.recent_fills.len();
    let position_qty = before.runtime.position_qty;
    let mark_price = before.runtime.mark_price;

    let accepted = engine
        .submit_command(
            CommandType::ShutdownAfterFlatten,
            CommandRequest {
                command_id: "cmd_shutdown_fill".into(),
            },
        )
        .await?;

    assert_eq!(accepted.status, CommandStatus::Accepted);

    let event = timeout(Duration::from_secs(1), events_rx.recv())
        .await?
        .expect("engine event");
    match event.event {
        EngineEvent::CommandAck(ack) => {
            assert_eq!(ack.command_id, "cmd_shutdown_fill");
            assert_eq!(ack.command, CommandType::ShutdownAfterFlatten);
            assert_eq!(ack.status, CommandStatus::Completed);
            assert_eq!(
                ack.links.client_order_ids,
                vec![
                    "grid_buy_01".to_string(),
                    "grid_sell_01".to_string(),
                    "reduce_only_cmd_shutdown_fill".to_string(),
                ]
            );
            assert_eq!(
                ack.links.order_ids,
                vec![
                    "ord_1001".to_string(),
                    "ord_1002".to_string(),
                    "order_cmd_shutdown_fill".to_string(),
                ]
            );
            assert_eq!(
                ack.links.trade_ids,
                vec!["trade_cmd_shutdown_fill".to_string()]
            );
        }
        other => panic!("unexpected engine event: {other:?}"),
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "paused");
    assert_eq!(snapshot.runtime.position_qty, 0.0);
    assert_close(
        snapshot.runtime.realized_pnl,
        before.runtime.realized_pnl
            + (mark_price - before.runtime.position_avg_price) * position_qty,
    );
    assert!(snapshot.execution.open_orders.is_empty());
    assert_eq!(
        snapshot.execution.recent_fills.len(),
        previous_fill_count + 1
    );

    let fill = snapshot
        .execution
        .recent_fills
        .first()
        .expect("shutdown flatten fill fact");
    assert_eq!(fill.side, "sell");
    assert_eq!(fill.qty, position_qty);
    assert_eq!(fill.price, mark_price);
    assert_close(
        fill.realized_pnl,
        (mark_price - before.runtime.position_avg_price) * position_qty,
    );

    Ok(())
}
fn assert_close(left: f64, right: f64) {
    assert!((left - right).abs() < 1e-9, "left={left} right={right}");
}
