use std::time::Duration;

use anyhow::Result;
use grid_platform_service::{
    kernel::{EngineEvent, spawn_engine},
    protocol::{CommandRequest, CommandStatus, CommandType},
};
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flatten_now_records_reduce_only_fill_in_execution_facts() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
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

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_after_flatten_pauses_strategy_and_records_fill_fact() -> Result<()> {
    let (engine, read_model, mut events_rx) = spawn_engine();
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

    Ok(())
}
