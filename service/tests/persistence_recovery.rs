use std::fs;
use std::time::Duration;

use anyhow::Result;
use grid_platform_service::{
    Application,
    protocol::{
        CommandAck, CommandRecord, CommandRequest, CommandStatus, CommandType, RuntimeSnapshot,
        SystemEvent,
    },
    storage::{PersistedRuntime, SqliteStorage},
};
use tempfile::tempdir;
use tokio::time::timeout;

#[test]
fn sqlite_storage_persists_command_audit_and_recovers_latest_runtime() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let storage = SqliteStorage::open(&db_path)?;

    let mut snapshot = RuntimeSnapshot::sample();
    snapshot.runtime.strategy_state = "paused".into();
    snapshot.runtime.last_price = 2368.88;
    snapshot.execution.open_orders.truncate(1);
    snapshot.execution.last_command_ack = Some("cmd_pause_storage".into());
    snapshot.execution.last_command_ack_event = Some(CommandAck {
        command_id: "cmd_pause_storage".into(),
        command: CommandType::Pause,
        status: CommandStatus::Completed,
        message: "Strategy paused.".into(),
        emitted_at: "2025-01-01T00:01:00Z".into(),
    });
    snapshot.execution.recent_commands = vec![CommandRecord {
        command_id: "cmd_pause_storage".into(),
        command: CommandType::Pause,
        status: CommandStatus::Completed,
        summary: "Strategy paused.".into(),
        requested_at: "2025-01-01T00:00:58Z".into(),
        accepted_at: Some("2025-01-01T00:00:59Z".into()),
        finished_at: Some("2025-01-01T00:01:00Z".into()),
    }];

    storage.persist_runtime(&PersistedRuntime {
        snapshot,
        risk_events: vec![],
        system_events: vec![SystemEvent {
            level: "info".into(),
            source: "commands".into(),
            message: "Strategy paused.".into(),
            created_at: "2025-01-01T00:01:00Z".into(),
        }],
        last_sequence: 7,
    })?;

    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let commands = storage.load_command_audit()?;

    assert_eq!(recovered.last_sequence, 7);
    assert_eq!(recovered.snapshot.runtime.strategy_state, "paused");
    assert_eq!(recovered.snapshot.runtime.last_price, 2368.88);
    assert_eq!(recovered.snapshot.execution.open_orders.len(), 1);
    assert_eq!(
        recovered.snapshot.execution.recent_commands[0].command_id,
        "cmd_pause_storage"
    );
    assert_eq!(recovered.system_events[0].source, "commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command_id, "cmd_pause_storage");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_bootstrap_uses_cold_start_message_for_empty_database() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;

    assert_eq!(application.system_events()[0].source, "bootstrap");
    assert_eq!(
        application.system_events()[0].message,
        "Rust runtime bootstrapped with SQLite storage."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_restart_recovers_latest_runtime_snapshot_from_sqlite() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;
    application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_restart".into(),
            },
        )
        .await?;
    let tick = application.emit_price_tick().await?;
    let snapshot_before_restart = application.snapshot();
    drop(application);

    let recovered = Application::bootstrap_with_sqlite(&db_path)?;
    let snapshot_after_restart = recovered.snapshot();

    assert_eq!(snapshot_after_restart.runtime.strategy_state, "paused");
    assert_eq!(snapshot_after_restart.runtime.last_price, tick.last_price);
    assert_eq!(
        snapshot_after_restart.runtime.last_price,
        snapshot_before_restart.runtime.last_price
    );
    assert!(
        snapshot_after_restart
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == "cmd_pause_restart")
    );
    assert_eq!(
        snapshot_after_restart.execution.recent_commands[0].command_id,
        "cmd_pause_restart"
    );
    assert_eq!(recovered.system_events()[0].source, "commands");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_storage_keeps_full_command_audit_beyond_recent_command_window() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");
    let application = Application::bootstrap_with_sqlite(&db_path)?;

    for index in 0..30 {
        let command = if index % 2 == 0 {
            CommandType::Pause
        } else {
            CommandType::Resume
        };
        application
            .submit_command(
                command,
                CommandRequest {
                    command_id: format!("cmd_audit_{index:02}"),
                },
            )
            .await?;
    }
    drop(application);

    let storage = SqliteStorage::open(&db_path)?;
    let recovered = storage
        .load_runtime()?
        .expect("runtime should be persisted");
    let audit = storage.load_command_audit()?;

    assert_eq!(audit.len(), 30);
    assert_eq!(audit[0].command_id, "cmd_audit_29");
    assert_eq!(
        audit.last().expect("oldest command").command_id,
        "cmd_audit_00"
    );
    assert_eq!(recovered.snapshot.execution.recent_commands.len(), 24);
    assert_eq!(
        recovered.snapshot.execution.recent_commands[0].command_id,
        "cmd_audit_29"
    );
    assert_eq!(
        recovered
            .snapshot
            .execution
            .recent_commands
            .last()
            .expect("recent command window tail")
            .command_id,
        "cmd_audit_06"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_persist_failure_rejects_command_and_rolls_back_runtime() -> Result<()> {
    let temp = tempdir()?;
    let db_path = temp.path().join("service.db");

    let application = Application::bootstrap_with_sqlite(&db_path)?;
    let mut events_rx = application.subscribe_events();

    fs::remove_file(&db_path)?;
    fs::create_dir(&db_path)?;

    let error = application
        .submit_command(
            CommandType::Pause,
            CommandRequest {
                command_id: "cmd_pause_persist_fail".into(),
            },
        )
        .await
        .expect_err("command should fail when sqlite persistence fails");
    assert!(
        error
            .to_string()
            .contains("failed to persist command result")
    );

    let snapshot = application.snapshot();
    assert_eq!(snapshot.runtime.strategy_state, "running");
    assert!(snapshot.execution.last_command_ack_event.is_none());
    assert!(snapshot.execution.recent_commands.is_empty());

    assert!(
        timeout(Duration::from_millis(200), events_rx.recv())
            .await
            .is_err()
    );

    Ok(())
}
