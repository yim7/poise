use anyhow::Result;
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};

use crate::protocol::{CommandLinks, CommandStatus, OpenOrder, RecentFill, RuntimeSnapshot};

#[derive(Debug, Clone, PartialEq)]
pub struct SubmitOrderRequest {
    pub command_id: Option<String>,
    pub order_id: String,
    pub client_order_id: String,
    pub side: String,
    pub price: f64,
    pub qty: f64,
    pub reduce_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubmitOrderResult {
    pub open_order: Option<OpenOrder>,
    pub fill: Option<RecentFill>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CancelOrdersRequest {
    pub command_id: Option<String>,
    pub order_ids: Vec<String>,
    pub client_order_ids: Vec<String>,
}

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult>;

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>>;

    async fn query_open_orders(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<OpenOrder>>;

    async fn list_recent_fills(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<RecentFill>>;
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
    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        if request.reduce_only {
            let trade_suffix = request.command_id.as_deref().unwrap_or(&request.order_id);
            return Ok(SubmitOrderResult {
                open_order: None,
                fill: Some(RecentFill {
                    trade_id: format!("trade_{trade_suffix}"),
                    order_id: request.order_id,
                    client_order_id: Some(request.client_order_id),
                    side: request.side,
                    price: request.price,
                    qty: request.qty,
                    fee: 0.0,
                    realized_pnl: 0.0,
                    event_time: now_utc(),
                }),
            });
        }

        if let Some(existing) = snapshot.execution.open_orders.iter().find(|order| {
            order.order_id == request.order_id || order.client_order_id == request.client_order_id
        }) {
            return Ok(SubmitOrderResult {
                open_order: Some(existing.clone()),
                fill: None,
            });
        }

        Ok(SubmitOrderResult {
            open_order: Some(OpenOrder {
                order_id: request.order_id,
                client_order_id: request.client_order_id,
                side: request.side,
                price: request.price,
                qty: request.qty,
                filled_qty: 0.0,
                status: "NEW".into(),
                created_at: now_utc(),
                updated_at: now_utc(),
            }),
            fill: None,
        })
    }

    async fn cancel_orders(
        &self,
        request: CancelOrdersRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<Vec<OpenOrder>> {
        Ok(snapshot
            .execution
            .open_orders
            .iter()
            .filter(|order| {
                !request.order_ids.iter().any(|id| id == &order.order_id)
                    && !request
                        .client_order_ids
                        .iter()
                        .any(|id| id == &order.client_order_id)
            })
            .cloned()
            .collect())
    }

    async fn query_open_orders(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<OpenOrder>> {
        Ok(snapshot.execution.open_orders.clone())
    }

    async fn list_recent_fills(&self, snapshot: &RuntimeSnapshot) -> Result<Vec<RecentFill>> {
        Ok(snapshot.execution.recent_fills.clone())
    }
}

pub fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}
