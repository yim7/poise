use std::time::Duration;

use anyhow::Result;
use grid_platform_service::{
    kernel::{EngineEvent, spawn_engine},
    protocol::{CommandRequest, CommandStatus, CommandType},
};
use tokio::time::timeout;

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
