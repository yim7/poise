use anyhow::Result;
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};

use crate::protocol::{CommandLinks, CommandStatus, OpenOrder, RecentFill, RuntimeSnapshot};

const EPSILON: f64 = 1e-9;

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

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PaperFillMarketUpdate {
    pub last_price: Option<f64>,
    pub mark_price: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    External,
    Paper,
}

#[async_trait]
pub trait ExecutionAdapter: Send + Sync {
    fn mode(&self) -> ExecutionMode {
        ExecutionMode::External
    }

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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionStatePatch {
    pub open_orders: Option<Vec<OpenOrder>>,
    pub recent_fills: Vec<RecentFill>,
    pub runtime_patch: ExecutionRuntimePatch,
}

impl ExecutionStatePatch {
    pub fn is_noop(&self) -> bool {
        self.open_orders.is_none()
            && self.recent_fills.is_empty()
            && self.runtime_patch == ExecutionRuntimePatch::default()
    }
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
    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Paper
    }

    async fn submit_order(
        &self,
        request: SubmitOrderRequest,
        snapshot: &RuntimeSnapshot,
    ) -> Result<SubmitOrderResult> {
        if request.reduce_only {
            let trade_suffix = request.command_id.as_deref().unwrap_or(&request.order_id);
            let fill_qty = reduce_only_fill_qty(snapshot, &request);
            let realized_pnl = reduce_only_realized_pnl(snapshot, &request, fill_qty);
            return Ok(SubmitOrderResult {
                open_order: None,
                fill: Some(RecentFill {
                    trade_id: format!("trade_{trade_suffix}"),
                    order_id: request.order_id,
                    client_order_id: Some(request.client_order_id),
                    side: request.side,
                    price: request.price,
                    qty: fill_qty,
                    fee: 0.0,
                    realized_pnl,
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

pub fn simulate_paper_fills(
    snapshot: &RuntimeSnapshot,
    market_update: PaperFillMarketUpdate,
    event_time: &str,
) -> ExecutionStatePatch {
    let Some(market_price) = market_price(market_update) else {
        return ExecutionStatePatch::default();
    };

    let mut next_open_orders = Vec::with_capacity(snapshot.execution.open_orders.len());
    let mut recent_fills = Vec::new();
    let mut position_qty = snapshot.runtime.position_qty;
    let mut position_avg_price = snapshot.runtime.position_avg_price;
    let mut realized_pnl = snapshot.runtime.realized_pnl;

    for order in &snapshot.execution.open_orders {
        let remaining_qty = (order.qty - order.filled_qty).max(0.0);
        if remaining_qty <= EPSILON {
            continue;
        }

        if should_fill_order(order, market_price) {
            let fill_realized_pnl = apply_fill_to_runtime(
                &mut position_qty,
                &mut position_avg_price,
                &mut realized_pnl,
                &order.side,
                order.price,
                remaining_qty,
            );
            recent_fills.push(RecentFill {
                trade_id: format!("paper_{}_{}", order.order_id, recent_fills.len() + 1),
                order_id: order.order_id.clone(),
                client_order_id: Some(order.client_order_id.clone()),
                side: order.side.clone(),
                price: order.price,
                qty: remaining_qty,
                fee: 0.0,
                realized_pnl: fill_realized_pnl,
                event_time: event_time.into(),
            });
        } else {
            next_open_orders.push(order.clone());
        }
    }

    let next_unrealized_pnl = unrealized_pnl(position_qty, position_avg_price, market_price);
    let unrealized_changed =
        (snapshot.runtime.unrealized_pnl - next_unrealized_pnl).abs() > EPSILON;

    if recent_fills.is_empty() && !unrealized_changed {
        return ExecutionStatePatch::default();
    }

    let mut patch = ExecutionStatePatch {
        open_orders: None,
        recent_fills,
        runtime_patch: ExecutionRuntimePatch {
            strategy_state: None,
            position_qty: None,
            position_avg_price: None,
            unrealized_pnl: None,
            realized_pnl: None,
        },
    };

    if !patch.recent_fills.is_empty() {
        patch.open_orders = Some(next_open_orders);
        patch.runtime_patch.position_qty = Some(position_qty);
        patch.runtime_patch.position_avg_price = Some(position_avg_price);
        patch.runtime_patch.unrealized_pnl = Some(next_unrealized_pnl);
        patch.runtime_patch.realized_pnl = Some(realized_pnl);
    } else if unrealized_changed {
        patch.runtime_patch.unrealized_pnl = Some(next_unrealized_pnl);
    }

    patch
}

fn market_price(market_update: PaperFillMarketUpdate) -> Option<f64> {
    market_update
        .mark_price
        .filter(|price| price.abs() > EPSILON)
        .or_else(|| {
            market_update
                .last_price
                .filter(|price| price.abs() > EPSILON)
        })
}

fn should_fill_order(order: &OpenOrder, market_price: f64) -> bool {
    match order.side.as_str() {
        "buy" => market_price <= order.price + EPSILON,
        "sell" => market_price + EPSILON >= order.price,
        _ => false,
    }
}

pub(crate) fn apply_fill_to_runtime(
    position_qty: &mut f64,
    position_avg_price: &mut f64,
    realized_pnl: &mut f64,
    side: &str,
    fill_price: f64,
    fill_qty: f64,
) -> f64 {
    let signed_fill_qty = match side {
        "buy" => fill_qty,
        "sell" => -fill_qty,
        _ => 0.0,
    };
    if signed_fill_qty.abs() <= EPSILON {
        return 0.0;
    }

    if position_qty.abs() <= EPSILON {
        *position_qty = signed_fill_qty;
        *position_avg_price = fill_price;
        return 0.0;
    }

    if position_qty.signum() == signed_fill_qty.signum() {
        let next_qty = *position_qty + signed_fill_qty;
        let gross_notional = position_qty.abs() * *position_avg_price + fill_qty * fill_price;
        *position_qty = next_qty;
        *position_avg_price = if next_qty.abs() <= EPSILON {
            0.0
        } else {
            gross_notional / next_qty.abs()
        };
        return 0.0;
    }

    let close_qty = position_qty.abs().min(fill_qty);
    let fill_realized_pnl = if *position_qty > 0.0 {
        (fill_price - *position_avg_price) * close_qty
    } else {
        (*position_avg_price - fill_price) * close_qty
    };
    *realized_pnl += fill_realized_pnl;

    let next_qty = *position_qty + signed_fill_qty;
    if next_qty.abs() <= EPSILON {
        *position_qty = 0.0;
        *position_avg_price = 0.0;
        return fill_realized_pnl;
    }

    if position_qty.signum() == next_qty.signum() {
        *position_qty = next_qty;
        return fill_realized_pnl;
    }

    *position_qty = next_qty;
    *position_avg_price = fill_price;
    fill_realized_pnl
}

fn unrealized_pnl(position_qty: f64, position_avg_price: f64, market_price: f64) -> f64 {
    if position_qty.abs() <= EPSILON || position_avg_price.abs() <= EPSILON {
        return 0.0;
    }

    if position_qty > 0.0 {
        (market_price - position_avg_price) * position_qty
    } else {
        (position_avg_price - market_price) * position_qty.abs()
    }
}

fn reduce_only_fill_qty(snapshot: &RuntimeSnapshot, request: &SubmitOrderRequest) -> f64 {
    snapshot.runtime.position_qty.abs().min(request.qty.abs())
}

fn reduce_only_realized_pnl(
    snapshot: &RuntimeSnapshot,
    request: &SubmitOrderRequest,
    fill_qty: f64,
) -> f64 {
    if fill_qty <= EPSILON {
        return 0.0;
    }

    match (
        snapshot.runtime.position_qty.partial_cmp(&0.0),
        request.side.as_str(),
    ) {
        (Some(std::cmp::Ordering::Greater), "sell") => {
            (request.price - snapshot.runtime.position_avg_price) * fill_qty
        }
        (Some(std::cmp::Ordering::Less), "buy") => {
            (snapshot.runtime.position_avg_price - request.price) * fill_qty
        }
        _ => 0.0,
    }
}
