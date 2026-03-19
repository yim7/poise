use crate::{
    protocol::{CommandType, OpenOrder, RecentFill},
    state::{AppState, CommandTimelineEntry, CommandTimelineStage},
};

#[derive(Debug, Clone, PartialEq)]
pub struct DashboardViewModel {
    pub symbol: String,
    pub strategy_state: String,
    pub session_state: String,
    pub position_qty: String,
    pub position_avg_price: String,
    pub unrealized_pnl: String,
    pub realized_pnl: String,
    pub open_orders: usize,
    pub pending_commands: usize,
    pub fills: usize,
    pub risk_summary: String,
    pub ws_summary: String,
}

pub fn dashboard(state: &AppState) -> DashboardViewModel {
    let health = connection_health(state);
    DashboardViewModel {
        symbol: state.runtime.symbol.clone(),
        strategy_state: state.runtime.strategy_state.clone(),
        session_state: state.runtime.session_state.clone(),
        position_qty: format!("{:.3}", state.runtime.position_qty),
        position_avg_price: format!("{:.2}", state.runtime.position_avg_price),
        unrealized_pnl: format!("{:.2}", state.runtime.unrealized_pnl),
        realized_pnl: format!("{:.2}", state.runtime.realized_pnl),
        open_orders: state.execution.open_orders.len(),
        pending_commands: state.execution.pending_commands.len(),
        fills: state.execution.recent_fills.len(),
        risk_summary: format!(
            "{:?} {:.0}/{:.0}",
            state.risk.risk_level, state.risk.current_notional, state.risk.max_notional
        ),
        ws_summary: format!("{} · {}", health.label, health.detail),
    }
}

pub fn dashboard_health_detail(state: &AppState) -> String {
    let health = connection_health(state);
    let latency_ms = state.connection.latency_ms.unwrap_or_default();
    let stale_age_ms = state.connection.stale_age_ms;

    match health.kind {
        ConnectionHealthKind::Healthy => format!("{latency_ms}ms stale {stale_age_ms}ms"),
        ConnectionHealthKind::Reconnecting => {
            if !state.connection.ws_connected {
                format!("svc retry {}ms", state.connection.reconnect_backoff_ms)
            } else {
                format!(
                    "mkt retry {}ms",
                    state.connection.market_reconnect_backoff_ms
                )
            }
        }
        ConnectionHealthKind::Stale => format!("stale {stale_age_ms}ms"),
        ConnectionHealthKind::Degraded => {
            if !state.connection.http_available {
                "http down".into()
            } else if state.connection.user_stream_connected == Some(false) {
                "user down".into()
            } else if latency_ms >= 750 {
                format!("lat {latency_ms}ms")
            } else {
                format!("stale {stale_age_ms}ms")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrderItemViewModel {
    pub side: String,
    pub price: String,
    pub qty: String,
    pub status: String,
    pub command_ref: Option<String>,
}

pub fn open_order_items(state: &AppState, limit: usize) -> Vec<OpenOrderItemViewModel> {
    state
        .execution
        .open_orders
        .iter()
        .take(limit)
        .map(|order| OpenOrderItemViewModel {
            side: order.side.to_uppercase(),
            price: format!("{:.2}", order.price),
            qty: format!("{:.3}", order.qty),
            status: order.status.clone(),
            command_ref: linked_command_for_order(state, order)
                .map(|entry| format!("{} {}", command_label(entry.command), entry.command_id)),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecentFillItemViewModel {
    pub side: String,
    pub price_qty: String,
    pub pnl: String,
    pub realized_pnl: f64,
    pub command_ref: Option<String>,
}

pub fn recent_fill_items(state: &AppState, limit: usize) -> Vec<RecentFillItemViewModel> {
    state
        .execution
        .recent_fills
        .iter()
        .take(limit)
        .map(|fill| RecentFillItemViewModel {
            side: fill.side.to_uppercase(),
            price_qty: format!("{:.2} x {:.3}", fill.price, fill.qty),
            pnl: format!("{:+.2}", fill.realized_pnl),
            realized_pnl: fill.realized_pnl,
            command_ref: linked_command_for_fill(state, fill)
                .map(|entry| format!("{} {}", command_label(entry.command), entry.command_id)),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridLevelViewModel {
    pub side: String,
    pub price: String,
    pub qty: String,
    pub distance_bps: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridViewModel {
    pub lower: String,
    pub upper: String,
    pub center: String,
    pub span_pct: String,
    pub active_levels: usize,
    pub buy_levels: usize,
    pub sell_levels: usize,
    pub inventory_bias: String,
    pub levels: Vec<GridLevelViewModel>,
}

pub fn grid(state: &AppState) -> GridViewModel {
    let lower = state
        .execution
        .open_orders
        .iter()
        .map(|order| order.price)
        .reduce(f64::min)
        .unwrap_or(state.runtime.last_price);
    let upper = state
        .execution
        .open_orders
        .iter()
        .map(|order| order.price)
        .reduce(f64::max)
        .unwrap_or(state.runtime.last_price);
    let center = (lower + upper) / 2.0;
    let span_pct = if center.abs() > f64::EPSILON {
        ((upper - lower) / center) * 100.0
    } else {
        0.0
    };
    let buy_levels = state
        .execution
        .open_orders
        .iter()
        .filter(|order| order.side.eq_ignore_ascii_case("buy"))
        .count();
    let sell_levels = state.execution.open_orders.len().saturating_sub(buy_levels);

    let mut orders = state.execution.open_orders.clone();
    orders.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    GridViewModel {
        lower: format!("{lower:.2}"),
        upper: format!("{upper:.2}"),
        center: format!("{center:.2}"),
        span_pct: format!("{span_pct:.2}%"),
        active_levels: orders.len(),
        buy_levels,
        sell_levels,
        inventory_bias: if state.runtime.position_qty > 0.0 {
            "long inventory".into()
        } else if state.runtime.position_qty < 0.0 {
            "short inventory".into()
        } else {
            "flat inventory".into()
        },
        levels: orders
            .iter()
            .map(|order| grid_level(order, state.runtime.last_price))
            .collect(),
    }
}

fn grid_level(order: &OpenOrder, last_price: f64) -> GridLevelViewModel {
    let distance_bps = if last_price.abs() > f64::EPSILON {
        ((order.price - last_price) / last_price) * 10_000.0
    } else {
        0.0
    };
    GridLevelViewModel {
        side: order.side.to_uppercase(),
        price: format!("{:.2}", order.price),
        qty: format!("{:.3}", order.qty),
        distance_bps: format!("{distance_bps:+.1}"),
        status: order.status.clone(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketViewModel {
    pub last_price: String,
    pub mark_price: String,
    pub basis: String,
    pub session_state: String,
    pub http_status: String,
    pub market_ws_status: String,
    pub user_stream_status: String,
    pub service_ws_status: String,
    pub latency: String,
    pub stale_age: String,
    pub reconnect_attempt: String,
    pub market_backoff: String,
    pub heartbeat: String,
}

pub fn market(state: &AppState) -> MarketViewModel {
    MarketViewModel {
        last_price: format!("{:.2}", state.runtime.last_price),
        mark_price: format!("{:.2}", state.runtime.mark_price),
        basis: format!(
            "{:+.2}",
            state.runtime.mark_price - state.runtime.last_price
        ),
        session_state: state.runtime.session_state.clone(),
        http_status: if state.connection.http_available {
            "UP"
        } else {
            "DOWN"
        }
        .into(),
        market_ws_status: if state.connection.market_ws_connected {
            "UP"
        } else {
            "DOWN"
        }
        .into(),
        user_stream_status: match state.connection.user_stream_connected {
            Some(true) => "UP",
            Some(false) => "DOWN",
            None => "N/A",
        }
        .into(),
        service_ws_status: if state.connection.ws_connected {
            "UP"
        } else {
            "DOWN"
        }
        .into(),
        latency: format!("{}ms", state.connection.latency_ms.unwrap_or_default()),
        stale_age: format!("{}ms", state.connection.stale_age_ms),
        reconnect_attempt: state.connection.reconnect_attempt.to_string(),
        market_backoff: format!("{}ms", state.connection.market_reconnect_backoff_ms),
        heartbeat: state.connection.last_heartbeat_at.clone(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventsViewModel {
    pub fills_count: usize,
    pub alerts_count: usize,
    pub system_count: usize,
    pub pending_commands: usize,
    pub timeline_count: usize,
}

pub fn events(state: &AppState) -> EventsViewModel {
    EventsViewModel {
        fills_count: state.execution.recent_fills.len(),
        alerts_count: state.risk.alerts.len(),
        system_count: state.system_events.len(),
        pending_commands: state.execution.pending_commands.len(),
        timeline_count: state.execution.command_timeline.len(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionHealthKind {
    Healthy,
    Degraded,
    Stale,
    Reconnecting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionHealthViewModel {
    pub label: &'static str,
    pub detail: String,
    pub hint: String,
    pub kind: ConnectionHealthKind,
}

pub fn connection_health(state: &AppState) -> ConnectionHealthViewModel {
    let latency_ms = state.connection.latency_ms.unwrap_or_default();
    let stale_age_ms = state.connection.stale_age_ms;

    if !state.connection.ws_connected {
        return ConnectionHealthViewModel {
            label: "RECONNECTING",
            detail: format!(
                "service ws retry {} in {}ms",
                state.connection.reconnect_attempt.max(1),
                state.connection.reconnect_backoff_ms
            ),
            hint: "Service control-plane WebSocket is down. Wait for the client stream to recover."
                .into(),
            kind: ConnectionHealthKind::Reconnecting,
        };
    }

    if !state.connection.market_ws_connected {
        return ConnectionHealthViewModel {
            label: "RECONNECTING",
            detail: format!(
                "binance ws retry in {}ms",
                state.connection.market_reconnect_backoff_ms
            ),
            hint: "Service is online, but Binance market stream is reconnecting. Treat market data as stale."
                .into(),
            kind: ConnectionHealthKind::Reconnecting,
        };
    }

    if stale_age_ms >= 8_000 {
        return ConnectionHealthViewModel {
            label: "STALE",
            detail: format!("feed lag {}ms", stale_age_ms),
            hint: "Heartbeat is alive but market data is not advancing. Recheck before trading."
                .into(),
            kind: ConnectionHealthKind::Stale,
        };
    }

    if !state.connection.http_available
        || latency_ms >= 750
        || stale_age_ms >= 3_000
        || state.connection.user_stream_connected == Some(false)
    {
        return ConnectionHealthViewModel {
            label: "DEGRADED",
            detail: format!(
                "http {} / {}ms / stale {}ms / user {}",
                status_word(state.connection.http_available),
                latency_ms,
                stale_age_ms,
                optional_status_word(state.connection.user_stream_connected)
            ),
            hint: "Connection is usable but lagging. Avoid risky commands until it settles.".into(),
            kind: ConnectionHealthKind::Degraded,
        };
    }

    ConnectionHealthViewModel {
        label: "HEALTHY",
        detail: format!("{}ms / stale {}ms", latency_ms, stale_age_ms),
        hint: "Service and Binance streams are both healthy.".into(),
        kind: ConnectionHealthKind::Healthy,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandTimelineItemViewModel {
    pub command_id: String,
    pub command_label: &'static str,
    pub stage_label: &'static str,
    pub stage: CommandTimelineStage,
    pub summary: String,
    pub timing: String,
}

pub fn command_timeline(state: &AppState, limit: usize) -> Vec<CommandTimelineItemViewModel> {
    state
        .execution
        .command_timeline
        .iter()
        .take(limit)
        .map(|entry| {
            let timing = match (&entry.accepted_at, &entry.finished_at) {
                (Some(accepted_at), Some(finished_at)) => format!(
                    "req {} -> acc {} -> end {}",
                    entry.requested_at, accepted_at, finished_at
                ),
                (Some(accepted_at), None) => {
                    format!("req {} -> acc {}", entry.requested_at, accepted_at)
                }
                (None, Some(finished_at)) => {
                    format!("req {} -> end {}", entry.requested_at, finished_at)
                }
                (None, None) => format!("req {}", entry.requested_at),
            };

            CommandTimelineItemViewModel {
                command_id: entry.command_id.clone(),
                command_label: command_label(entry.command),
                stage_label: entry.stage.label(),
                stage: entry.stage,
                summary: entry.summary.clone(),
                timing,
            }
        })
        .collect()
}

pub fn command_label(command: CommandType) -> &'static str {
    match command {
        CommandType::Pause => "PAUSE",
        CommandType::Resume => "RESUME",
        CommandType::CancelAll => "CANCEL ALL",
        CommandType::FlattenNow => "FLATTEN NOW",
        CommandType::ShutdownAfterFlatten => "SHUTDOWN",
    }
}

fn linked_command_for_order<'a>(
    state: &'a AppState,
    order: &OpenOrder,
) -> Option<&'a CommandTimelineEntry> {
    state.execution.command_timeline.iter().find(|entry| {
        entry.links.order_ids.iter().any(|id| id == &order.order_id)
            || entry
                .links
                .client_order_ids
                .iter()
                .any(|id| id == &order.client_order_id)
    })
}

fn linked_command_for_fill<'a>(
    state: &'a AppState,
    fill: &RecentFill,
) -> Option<&'a CommandTimelineEntry> {
    state.execution.command_timeline.iter().find(|entry| {
        entry.links.trade_ids.iter().any(|id| id == &fill.trade_id)
            || entry.links.order_ids.iter().any(|id| id == &fill.order_id)
            || fill
                .client_order_id
                .as_ref()
                .is_some_and(|client_order_id| {
                    entry
                        .links
                        .client_order_ids
                        .iter()
                        .any(|id| id == client_order_id)
                })
    })
}

fn status_word(is_up: bool) -> &'static str {
    if is_up { "up" } else { "down" }
}

fn optional_status_word(is_up: Option<bool>) -> &'static str {
    match is_up {
        Some(true) => "up",
        Some(false) => "down",
        None => "n/a",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::CommandLinks, state::AppState};

    #[test]
    fn degraded_connection_detail_uses_na_for_unconfigured_user_stream() {
        let mut state = AppState::sample();
        state.connection.ws_connected = true;
        state.connection.market_ws_connected = true;
        state.connection.http_available = false;
        state.connection.user_stream_connected = None;

        let health = connection_health(&state);

        assert_eq!(health.kind, ConnectionHealthKind::Degraded);
        assert!(health.detail.contains("user n/a"));
    }

    #[test]
    fn open_order_and_fill_items_surface_linked_command_refs() {
        let mut state = AppState::sample();
        state
            .execution
            .command_timeline
            .push_front(CommandTimelineEntry {
                command_id: "cmd_cancel_01".into(),
                command: CommandType::CancelAll,
                stage: CommandTimelineStage::Ack,
                summary: "All open orders cancelled.".into(),
                requested_at: "2025-01-01T00:00:03Z".into(),
                accepted_at: Some("2025-01-01T00:00:04Z".into()),
                finished_at: Some("2025-01-01T00:00:05Z".into()),
                links: CommandLinks {
                    client_order_ids: vec!["grid_buy_01".into()],
                    order_ids: vec!["ord_1001".into()],
                    trade_ids: vec!["fill_9001".into()],
                },
                timeout_at_tick: None,
            });

        let order_items = open_order_items(&state, 4);
        let fill_items = recent_fill_items(&state, 4);

        assert_eq!(
            order_items[0].command_ref.as_deref(),
            Some("CANCEL ALL cmd_cancel_01")
        );
        assert_eq!(
            fill_items[0].command_ref.as_deref(),
            Some("CANCEL ALL cmd_cancel_01")
        );
    }
}
