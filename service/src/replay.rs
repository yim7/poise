use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::timeout};

use crate::{
    execution::ExecutionAdapter,
    kernel::{SharedReadModel, spawn_engine_with_runtime_and_adapter},
    protocol::{CommandRequest, CommandType, RuntimeSnapshot},
    storage::PersistedRuntime,
};

const REPLAY_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayScenario {
    pub name: String,
    #[serde(default)]
    pub steps: Vec<ReplayStep>,
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
    },
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
                wait_for_command_ack(&read_model, &mut events_rx, command_id).await?;
            }
        }
    }

    Ok(ReplayRunResult {
        snapshot: read_model.read().expect("read model").snapshot(),
        replayed_steps: scenario.steps.len(),
    })
}

async fn wait_for_command_ack(
    read_model: &SharedReadModel,
    events_rx: &mut mpsc::Receiver<crate::kernel::SequencedEngineEvent>,
    command_id: &str,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + REPLAY_COMMAND_TIMEOUT;
    loop {
        let snapshot = read_model.read().expect("read model").snapshot();
        if snapshot
            .execution
            .last_command_ack_event
            .as_ref()
            .is_some_and(|ack| ack.command_id == command_id)
            && snapshot
                .execution
                .pending_commands
                .iter()
                .all(|command| command.command_id != command_id)
        {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for replay command ack {command_id}"
            ));
        }

        let _ = timeout(Duration::from_millis(50), events_rx.recv()).await;
    }
}
