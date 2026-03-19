use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};

use crate::protocol::{
    CommandLinks, CommandStatus, CommandType, OpenOrder, RecentFill, RuntimeSnapshot,
};

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    async fn execute(
        &self,
        command: CommandType,
        command_id: &str,
        snapshot: &RuntimeSnapshot,
    ) -> Result<ExecutionOutcome>;
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionRuntimePatch {
    pub strategy_state: Option<String>,
    pub position_qty: Option<f64>,
    pub position_avg_price: Option<f64>,
    pub unrealized_pnl: Option<f64>,
    pub realized_pnl: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionOutcome {
    pub status: CommandStatus,
    pub summary: String,
    pub open_orders: Option<Vec<OpenOrder>>,
    pub recent_fills: Option<Vec<RecentFill>>,
    pub links: CommandLinks,
    pub runtime_patch: ExecutionRuntimePatch,
}

impl ExecutionOutcome {
    pub fn completed(summary: impl Into<String>) -> Self {
        Self {
            status: CommandStatus::Completed,
            summary: summary.into(),
            open_orders: None,
            recent_fills: None,
            links: CommandLinks::default(),
            runtime_patch: ExecutionRuntimePatch::default(),
        }
    }

    pub fn failed(summary: impl Into<String>) -> Self {
        Self {
            status: CommandStatus::Failed,
            summary: summary.into(),
            open_orders: None,
            recent_fills: None,
            links: CommandLinks::default(),
            runtime_patch: ExecutionRuntimePatch::default(),
        }
    }

    pub fn timed_out(summary: impl Into<String>) -> Self {
        Self {
            status: CommandStatus::TimedOut,
            summary: summary.into(),
            open_orders: None,
            recent_fills: None,
            links: CommandLinks::default(),
            runtime_patch: ExecutionRuntimePatch::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeExecutionAdapter;

#[async_trait]
impl ExecutionAdapter for FakeExecutionAdapter {
    async fn execute(
        &self,
        command: CommandType,
        command_id: &str,
        snapshot: &RuntimeSnapshot,
    ) -> Result<ExecutionOutcome> {
        let outcome = match command {
            CommandType::CancelAll => {
                let mut outcome = ExecutionOutcome::completed("All open orders cancelled.");
                outcome.links.client_order_ids = snapshot
                    .execution
                    .open_orders
                    .iter()
                    .map(|order| order.client_order_id.clone())
                    .collect();
                outcome.links.order_ids = snapshot
                    .execution
                    .open_orders
                    .iter()
                    .map(|order| order.order_id.clone())
                    .collect();
                outcome.open_orders = Some(Vec::new());
                outcome
            }
            CommandType::FlattenNow => flatten_outcome(snapshot, command_id, false),
            CommandType::ShutdownAfterFlatten => flatten_outcome(snapshot, command_id, true),
            CommandType::Pause | CommandType::Resume => {
                unreachable!("local runtime commands do not use the execution adapter")
            }
        };
        Ok(outcome)
    }
}

#[derive(Debug, Default)]
pub struct ScriptedExecutionAdapter {
    outcomes: Mutex<HashMap<String, ExecutionOutcome>>,
}

impl ScriptedExecutionAdapter {
    pub fn new() -> Self {
        Self {
            outcomes: Mutex::new(HashMap::new()),
        }
    }

    pub fn push_outcome(&self, command_id: impl Into<String>, outcome: ExecutionOutcome) {
        let mut outcomes = self.outcomes.lock().expect("scripted adapter poisoned");
        outcomes.insert(command_id.into(), outcome);
    }
}

#[async_trait]
impl ExecutionAdapter for ScriptedExecutionAdapter {
    async fn execute(
        &self,
        _command: CommandType,
        command_id: &str,
        _snapshot: &RuntimeSnapshot,
    ) -> Result<ExecutionOutcome> {
        let mut outcomes = self.outcomes.lock().expect("scripted adapter poisoned");
        if let Some(outcome) = outcomes.remove(command_id) {
            return Ok(outcome);
        }
        bail!("no scripted outcome for command_id {command_id}")
    }
}

fn flatten_outcome(
    snapshot: &RuntimeSnapshot,
    command_id: &str,
    pause_after_flatten: bool,
) -> ExecutionOutcome {
    let mut outcome = ExecutionOutcome::completed(if pause_after_flatten {
        "Position flattened and shutdown requested."
    } else {
        "Position flattened."
    });

    outcome.runtime_patch.position_qty = Some(0.0);
    outcome.runtime_patch.position_avg_price = Some(0.0);
    outcome.runtime_patch.unrealized_pnl = Some(0.0);

    if pause_after_flatten {
        outcome.runtime_patch.strategy_state = Some("paused".into());
        outcome.links.client_order_ids.extend(
            snapshot
                .execution
                .open_orders
                .iter()
                .map(|order| order.client_order_id.clone()),
        );
        outcome.links.order_ids.extend(
            snapshot
                .execution
                .open_orders
                .iter()
                .map(|order| order.order_id.clone()),
        );
        outcome.open_orders = Some(Vec::new());
    }

    let qty = snapshot.runtime.position_qty.abs();
    if qty <= f64::EPSILON {
        return outcome;
    }

    let side = if snapshot.runtime.position_qty > 0.0 {
        "sell"
    } else {
        "buy"
    };
    let price = if snapshot.runtime.mark_price > 0.0 {
        snapshot.runtime.mark_price
    } else {
        snapshot.runtime.last_price
    };
    let client_order_id = format!("reduce_only_{command_id}");
    let order_id = format!("order_{command_id}");
    let trade_id = format!("trade_{command_id}");

    let mut recent_fills = Vec::with_capacity(snapshot.execution.recent_fills.len() + 1);
    recent_fills.push(RecentFill {
        trade_id: trade_id.clone(),
        order_id: order_id.clone(),
        client_order_id: Some(client_order_id.clone()),
        side: side.into(),
        price,
        qty,
        fee: 0.0,
        realized_pnl: 0.0,
        event_time: now_utc(),
    });
    recent_fills.extend(snapshot.execution.recent_fills.iter().cloned());
    outcome.recent_fills = Some(recent_fills);
    outcome.links.client_order_ids.push(client_order_id);
    outcome.links.order_ids.push(order_id);
    outcome.links.trade_ids.push(trade_id);
    outcome
}

fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}
