use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::timeout};

use crate::{
    execution::ExecutionAdapter,
    kernel::{SharedReadModel, spawn_engine_with_runtime_and_adapter},
    protocol::{CommandRequest, CommandStatus, CommandType, RuntimeSnapshot},
    storage::PersistedRuntime,
};

const REPLAY_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayScenario {
    pub name: String,
    #[serde(default)]
    pub steps: Vec<ReplayStep>,
    #[serde(default)]
    pub assertions: ReplayAssertions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplayStep {
    Market {
        last_price: Option<f64>,
        mark_price: Option<f64>,
        emitted_at: String,
    },
    Command {
        command: CommandType,
        command_id: String,
        #[serde(default)]
        expect_status: Option<CommandStatus>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayAssertions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_order_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_fill_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_status: Option<CommandStatus>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayRunResult {
    pub snapshot: RuntimeSnapshot,
    pub replayed_steps: usize,
}

pub async fn run_replay_scenario(
    runtime: PersistedRuntime,
    execution_adapter: Arc<dyn ExecutionAdapter>,
    scenario: &ReplayScenario,
) -> Result<ReplayRunResult> {
    let (engine, read_model, mut events_rx) =
        spawn_engine_with_runtime_and_adapter(runtime, None, execution_adapter);

    for step in &scenario.steps {
        match step {
            ReplayStep::Market {
                last_price,
                mark_price,
                emitted_at,
            } => {
                engine
                    .sync_market_prices(*last_price, *mark_price, emitted_at.clone())
                    .await
                    .with_context(|| format!("failed replay market step in {}", scenario.name))?;
            }
            ReplayStep::Command {
                command,
                command_id,
                expect_status,
            } => {
                engine
                    .submit_command(
                        *command,
                        CommandRequest {
                            command_id: command_id.clone(),
                        },
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "failed replay command step {} in {}",
                            command_id, scenario.name
                        )
                    })?;
                let ack_status =
                    wait_for_command_ack(&read_model, &mut events_rx, command_id).await?;
                if let Some(expected_status) = expect_status
                    && ack_status != *expected_status
                {
                    return Err(anyhow!(
                        "expected command status {:?} for {}, got {:?}",
                        expected_status,
                        command_id,
                        ack_status
                    ));
                }
            }
        }
    }

    let snapshot = read_model.read().expect("read model").snapshot();
    assert_replay_assertions(&snapshot, &scenario.assertions)?;

    Ok(ReplayRunResult {
        snapshot,
        replayed_steps: scenario.steps.len(),
    })
}

async fn wait_for_command_ack(
    read_model: &SharedReadModel,
    events_rx: &mut mpsc::Receiver<crate::kernel::SequencedEngineEvent>,
    command_id: &str,
) -> Result<CommandStatus> {
    let deadline = tokio::time::Instant::now() + REPLAY_COMMAND_TIMEOUT;
    loop {
        let snapshot = read_model.read().expect("read model").snapshot();
        if let Some(ack) = snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .filter(|ack| ack.command_id == command_id)
            && snapshot
                .execution
                .pending_commands
                .iter()
                .all(|command| command.command_id != command_id)
        {
            return Ok(ack.status);
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for replay command ack {command_id}"
            ));
        }

        let _ = timeout(Duration::from_millis(50), events_rx.recv()).await;
    }
}

fn assert_replay_assertions(
    snapshot: &RuntimeSnapshot,
    assertions: &ReplayAssertions,
) -> Result<()> {
    if let Some(position_qty) = assertions.position_qty
        && (snapshot.runtime.position_qty - position_qty).abs() > f64::EPSILON
    {
        return Err(anyhow!(
            "expected runtime.position_qty {}, got {}",
            position_qty,
            snapshot.runtime.position_qty
        ));
    }

    if let Some(open_order_count) = assertions.open_order_count
        && snapshot.execution.open_orders.len() != open_order_count
    {
        return Err(anyhow!(
            "expected execution.open_orders len {}, got {}",
            open_order_count,
            snapshot.execution.open_orders.len()
        ));
    }

    if let Some(recent_fill_count) = assertions.recent_fill_count
        && snapshot.execution.recent_fills.len() != recent_fill_count
    {
        return Err(anyhow!(
            "expected execution.recent_fills len {}, got {}",
            recent_fill_count,
            snapshot.execution.recent_fills.len()
        ));
    }

    if let Some(last_command_status) = assertions.last_command_status
        && snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .map(|ack| ack.status)
            != Some(last_command_status)
    {
        return Err(anyhow!(
            "expected last command status {:?}, got {:?}",
            last_command_status,
            snapshot
                .execution
                .last_command_ack_event
                .as_ref()
                .map(|ack| ack.status)
        ));
    }

    Ok(())
}
