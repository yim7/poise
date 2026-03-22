use crate::{
    locale::{self, Locale},
    protocol::{
        CommandType, GridLevel, GridLevelState, GridSide, OpenOrder, OpenOrdersSource, RecentFill,
    },
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
    pub exchange_orders: Option<usize>,
    pub pending_commands: usize,
    pub fills: usize,
    pub risk_summary: String,
    pub ws_summary: String,
}

pub fn dashboard(state: &AppState) -> DashboardViewModel {
    let copy = locale::copy(state.ui.locale);
    let health = connection_health(state);
    DashboardViewModel {
        symbol: state.runtime.symbol.clone(),
        strategy_state: state.runtime.strategy_state.clone(),
        session_state: state.runtime.session_state.clone(),
        position_qty: format!("{:.3}", state.runtime.position_qty),
        position_avg_price: format!("{:.2}", state.runtime.position_avg_price),
        unrealized_pnl: format!("{:.2}", state.runtime.unrealized_pnl),
        realized_pnl: format!("{:.2}", state.runtime.realized_pnl),
        exchange_orders: (state.execution.exchange_open_orders_source
            == OpenOrdersSource::ExchangeLive)
            .then_some(state.execution.exchange_open_orders.len()),
        pending_commands: state.execution.pending_commands.len(),
        fills: state.execution.recent_fills.len(),
        risk_summary: format!(
            "{} {:.0}/{:.0}",
            copy.dashboard().risk_level_label(state.risk.risk_level),
            state.risk.current_notional,
            state.risk.max_notional
        ),
        ws_summary: format!("{} · {}", health.label, health.detail),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceItemViewModel {
    pub symbol: String,
    pub environment: String,
    pub is_default: bool,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancesViewModel {
    pub environment: String,
    pub default_symbol: Option<String>,
    pub current_symbol: Option<String>,
    pub items: Vec<InstanceItemViewModel>,
}

pub fn instances(state: &AppState) -> InstancesViewModel {
    let current_symbol = state.instances.current_symbol.clone();
    let default_symbol = state.instances.default_symbol.clone();
    let mut items: Vec<_> = state
        .instances
        .items
        .iter()
        .map(|item| InstanceItemViewModel {
            symbol: item.symbol.clone(),
            environment: item.environment.clone(),
            is_default: item.is_default,
            is_current: current_symbol
                .as_deref()
                .is_some_and(|symbol| symbol == item.symbol),
        })
        .collect();
    items.sort_by(|a, b| {
        b.is_current
            .cmp(&a.is_current)
            .then(b.is_default.cmp(&a.is_default))
            .then(a.symbol.cmp(&b.symbol))
    });

    InstancesViewModel {
        environment: state.instances.environment.clone(),
        default_symbol,
        current_symbol,
        items,
    }
}

pub fn dashboard_health_detail(state: &AppState) -> String {
    let selector_copy = locale::copy(state.ui.locale).selector();
    let health = connection_health(state);
    let stale_age_ms = state.connection.stale_age_ms;

    match health.kind {
        ConnectionHealthKind::Healthy => selector_copy.healthy_detail(stale_age_ms),
        ConnectionHealthKind::Reconnecting => {
            if !state.connection.ws_connected {
                selector_copy.dashboard_service_retry_detail(state.connection.reconnect_backoff_ms)
            } else {
                selector_copy
                    .dashboard_market_retry_detail(state.connection.market_reconnect_backoff_ms)
            }
        }
        ConnectionHealthKind::Stale => selector_copy.healthy_detail(stale_age_ms),
        ConnectionHealthKind::Degraded => {
            if !state.connection.http_available {
                selector_copy.dashboard_http_down().into()
            } else if state.connection.user_stream_connected == Some(false) {
                selector_copy.dashboard_user_down().into()
            } else {
                selector_copy.healthy_detail(stale_age_ms)
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
        .exchange_open_orders
        .iter()
        .take(limit)
        .map(|order| OpenOrderItemViewModel {
            side: order.side.to_uppercase(),
            price: format!("{:.2}", order.price),
            qty: format!("{:.3}", order.qty),
            status: order.status.clone(),
            command_ref: linked_command_for_order(state, order).map(|entry| {
                format!(
                    "{} {}",
                    command_label(state.ui.locale, entry.command),
                    entry.command_id
                )
            }),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyOrderItemViewModel {
    pub side: String,
    pub price: String,
    pub qty: String,
    pub status: String,
}

pub fn strategy_orders(state: &AppState) -> Vec<StrategyOrderItemViewModel> {
    let mut orders = strategy_order_source(state)
        .iter()
        .filter(|order| is_strategy_managed_order(state, order))
        .cloned()
        .collect::<Vec<_>>();
    orders.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    orders
        .iter()
        .map(|order| StrategyOrderItemViewModel {
            side: order.side.to_uppercase(),
            price: format!("{:.2}", order.price),
            qty: format!("{:.3}", order.qty),
            status: order.status.clone(),
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
            command_ref: linked_command_for_fill(state, fill).map(|entry| {
                format!(
                    "{} {}",
                    command_label(state.ui.locale, entry.command),
                    entry.command_id
                )
            }),
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
    pub status: String,
    pub lower: String,
    pub upper: String,
    pub center: String,
    pub span_pct: String,
    pub active_levels: usize,
    pub occupied_levels: usize,
    pub pending_levels: usize,
    pub inventory_bias: String,
    pub pending_rebuild_reason: Option<String>,
    pub levels: Vec<GridLevelViewModel>,
}

pub fn grid(state: &AppState) -> GridViewModel {
    let selector_copy = locale::copy(state.ui.locale).selector();
    let center = state.strategy.center_price;
    let span_pct = if center.abs() > f64::EPSILON {
        ((state.strategy.upper_bound - state.strategy.lower_bound) / center) * 100.0
    } else {
        0.0
    };
    let mut levels = state.strategy.levels.clone();
    levels.sort_by(|a, b| {
        a.price
            .partial_cmp(&b.price)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    GridViewModel {
        status: selector_copy
            .strategy_status_label(state.strategy.status)
            .into(),
        lower: format!("{:.2}", state.strategy.lower_bound),
        upper: format!("{:.2}", state.strategy.upper_bound),
        center: format!("{center:.2}"),
        span_pct: format!("{span_pct:.2}%"),
        active_levels: levels
            .iter()
            .filter(|level| level.state == GridLevelState::Active)
            .count(),
        occupied_levels: levels
            .iter()
            .filter(|level| level.state == GridLevelState::Occupied)
            .count(),
        pending_levels: levels
            .iter()
            .filter(|level| level.state == GridLevelState::PendingRebuild)
            .count(),
        inventory_bias: if state.runtime.position_qty > 0.0 {
            selector_copy.long_inventory().into()
        } else if state.runtime.position_qty < 0.0 {
            selector_copy.short_inventory().into()
        } else {
            selector_copy.flat_inventory().into()
        },
        pending_rebuild_reason: state.strategy.status_reason.clone(),
        levels: levels
            .iter()
            .map(|level| grid_level(state.ui.locale, level, state.runtime.last_price))
            .collect(),
    }
}

fn grid_level(locale: Locale, level: &GridLevel, last_price: f64) -> GridLevelViewModel {
    let distance_bps = if last_price.abs() > f64::EPSILON {
        ((level.price - last_price) / last_price) * 10_000.0
    } else {
        0.0
    };
    GridLevelViewModel {
        side: match level.side {
            GridSide::Buy => "BUY".into(),
            GridSide::Sell => "SELL".into(),
        },
        price: format!("{:.2}", level.price),
        qty: format!("{:.3}", level.quantity),
        distance_bps: format!("{distance_bps:+.1}"),
        status: grid_level_state_label(locale, level.state).into(),
    }
}

fn strategy_order_source(state: &AppState) -> &[OpenOrder] {
    if state.execution.exchange_open_orders_source == OpenOrdersSource::ExchangeLive {
        &state.execution.exchange_open_orders
    } else {
        &[]
    }
}

fn is_strategy_managed_order(state: &AppState, order: &OpenOrder) -> bool {
    if order.client_order_id.starts_with("grid_") {
        return true;
    }

    state.strategy.levels.iter().any(|level| {
        level
            .client_order_id
            .as_ref()
            .is_some_and(|client_order_id| client_order_id == &order.client_order_id)
            || level
                .order_id
                .as_ref()
                .is_some_and(|order_id| order_id == &order.order_id)
    })
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
    pub stale_age: String,
    pub reconnect_attempt: String,
    pub market_backoff: String,
    pub heartbeat: String,
}

pub fn market(state: &AppState) -> MarketViewModel {
    let selector_copy = locale::copy(state.ui.locale).selector();
    MarketViewModel {
        last_price: format!("{:.2}", state.runtime.last_price),
        mark_price: format!("{:.2}", state.runtime.mark_price),
        basis: format!(
            "{:+.2}",
            state.runtime.mark_price - state.runtime.last_price
        ),
        session_state: state.runtime.session_state.clone(),
        http_status: selector_copy
            .market_status(state.connection.http_available)
            .into(),
        market_ws_status: selector_copy
            .market_status(state.connection.market_ws_connected)
            .into(),
        user_stream_status: match state.connection.user_stream_connected {
            status => selector_copy.optional_market_status(status),
        }
        .into(),
        service_ws_status: selector_copy
            .market_status(state.connection.ws_connected)
            .into(),
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
    let selector_copy = locale::copy(state.ui.locale).selector();
    let stale_age_ms = state.connection.stale_age_ms;

    if !state.connection.ws_connected {
        return ConnectionHealthViewModel {
            label: selector_copy.service_reconnecting_label(),
            detail: selector_copy.service_reconnecting_detail(
                state.connection.reconnect_attempt.max(1),
                state.connection.reconnect_backoff_ms,
            ),
            hint: selector_copy.service_reconnecting_hint().into(),
            kind: ConnectionHealthKind::Reconnecting,
        };
    }

    if !state.connection.market_ws_connected {
        return ConnectionHealthViewModel {
            label: selector_copy.market_reconnecting_label(),
            detail: selector_copy
                .market_reconnecting_detail(state.connection.market_reconnect_backoff_ms),
            hint: selector_copy.market_reconnecting_hint().into(),
            kind: ConnectionHealthKind::Reconnecting,
        };
    }

    if stale_age_ms >= 8_000 {
        return ConnectionHealthViewModel {
            label: selector_copy.stale_label(),
            detail: selector_copy.stale_detail(stale_age_ms),
            hint: selector_copy.stale_hint().into(),
            kind: ConnectionHealthKind::Stale,
        };
    }

    if !state.connection.http_available
        || stale_age_ms >= 3_000
        || state.connection.user_stream_connected == Some(false)
    {
        return ConnectionHealthViewModel {
            label: selector_copy.degraded_label(),
            detail: selector_copy.degraded_detail(
                state.connection.http_available,
                stale_age_ms,
                state.connection.user_stream_connected,
            ),
            hint: selector_copy.degraded_hint().into(),
            kind: ConnectionHealthKind::Degraded,
        };
    }

    ConnectionHealthViewModel {
        label: selector_copy.healthy_label(),
        detail: selector_copy.healthy_detail(stale_age_ms),
        hint: selector_copy.healthy_hint().into(),
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
    let selector_copy = locale::copy(state.ui.locale).selector();
    state
        .execution
        .command_timeline
        .iter()
        .take(limit)
        .map(|entry| CommandTimelineItemViewModel {
            command_id: entry.command_id.clone(),
            command_label: selector_copy.command_label(entry.command),
            stage_label: selector_copy.stage_label(entry.stage),
            stage: entry.stage,
            summary: entry.summary.clone(),
            timing: selector_copy.command_timing(
                &entry.requested_at,
                entry.accepted_at.as_deref(),
                entry.finished_at.as_deref(),
            ),
        })
        .collect()
}

pub fn command_label(locale: Locale, command: CommandType) -> &'static str {
    locale::copy(locale).selector().command_label(command)
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

pub fn risk_action_hint(locale: Locale, code: &str) -> &'static str {
    locale::copy(locale).selector().risk_action_hint(code)
}

fn grid_level_state_label(locale: Locale, state: GridLevelState) -> &'static str {
    locale::copy(locale)
        .selector()
        .grid_level_state_label(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::InstanceSummary;
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
    fn healthy_connection_detail_uses_stale_only() {
        let mut state = AppState::sample();
        state.connection.ws_connected = true;
        state.connection.market_ws_connected = true;
        state.connection.http_available = true;
        state.connection.user_stream_connected = Some(true);
        state.connection.stale_age_ms = 0;

        let health = connection_health(&state);
        let detail = dashboard_health_detail(&state);

        assert_eq!(health.kind, ConnectionHealthKind::Healthy);
        assert_eq!(health.detail, "stale 0ms");
        assert_eq!(detail, "stale 0ms");
    }

    #[test]
    fn healthy_connection_detail_uses_lagging_copy_in_chinese() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;
        state.connection.ws_connected = true;
        state.connection.market_ws_connected = true;
        state.connection.http_available = true;
        state.connection.user_stream_connected = Some(true);
        state.connection.stale_age_ms = 0;

        let health = connection_health(&state);
        let detail = dashboard_health_detail(&state);

        assert_eq!(health.kind, ConnectionHealthKind::Healthy);
        assert_eq!(health.detail, "滞后 0ms");
        assert_eq!(detail, "滞后 0ms");
    }

    #[test]
    fn service_reconnect_health_label_is_specific() {
        let mut state = AppState::sample();
        state.connection.ws_connected = false;
        state.connection.reconnect_attempt = 2;
        state.connection.reconnect_backoff_ms = 2_000;

        let health = connection_health(&state);

        assert_eq!(health.kind, ConnectionHealthKind::Reconnecting);
        assert_eq!(health.label, "SERVICE RECONNECTING");
    }

    #[test]
    fn market_reconnect_health_label_is_specific_when_service_ws_is_up() {
        let mut state = AppState::sample();
        state.connection.ws_connected = true;
        state.connection.market_ws_connected = false;
        state.connection.market_reconnect_backoff_ms = 2_000;

        let health = connection_health(&state);

        assert_eq!(health.kind, ConnectionHealthKind::Reconnecting);
        assert_eq!(health.label, "MARKET RECONNECTING");
    }

    #[test]
    fn open_order_and_fill_items_surface_linked_command_refs() {
        let mut state = AppState::sample();
        state.execution.exchange_open_orders_source = OpenOrdersSource::ExchangeLive;
        state.execution.exchange_open_orders = vec![OpenOrder {
            order_id: "ord_1001".into(),
            client_order_id: "grid_buy_01".into(),
            side: "buy".into(),
            price: 2332.80,
            qty: 0.100,
            filled_qty: 0.0,
            status: "NEW".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }];
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

    #[test]
    fn open_order_and_fill_items_localize_linked_command_refs() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;
        state.execution.exchange_open_orders_source = OpenOrdersSource::ExchangeLive;
        state.execution.exchange_open_orders = vec![OpenOrder {
            order_id: "ord_1001".into(),
            client_order_id: "grid_buy_01".into(),
            side: "buy".into(),
            price: 2332.80,
            qty: 0.100,
            filled_qty: 0.0,
            status: "NEW".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }];
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
        state.execution.recent_fills = vec![RecentFill {
            trade_id: "fill_9001".into(),
            order_id: "ord_1001".into(),
            client_order_id: Some("grid_buy_01".into()),
            side: "sell".into(),
            price: 2335.10,
            qty: 0.100,
            fee: 0.05,
            realized_pnl: 1.23,
            event_time: "2025-01-01T00:00:05Z".into(),
        }]
        .into();

        let order_items = open_order_items(&state, 4);
        let fill_items = recent_fill_items(&state, 4);

        assert_eq!(
            order_items[0].command_ref.as_deref(),
            Some("取消全部 cmd_cancel_01")
        );
        assert_eq!(
            fill_items[0].command_ref.as_deref(),
            Some("取消全部 cmd_cancel_01")
        );
    }

    #[test]
    fn instances_selector_marks_current_item_first() {
        let mut state = AppState::sample();
        state.instances.environment = "testnet".into();
        state.instances.default_symbol = Some("BTCUSDT".into());
        state.instances.current_symbol = Some("ETHUSDT".into());
        state.instances.items = vec![
            InstanceSummary {
                symbol: "BTCUSDT".into(),
                environment: "testnet".into(),
                is_default: true,
            },
            InstanceSummary {
                symbol: "ETHUSDT".into(),
                environment: "testnet".into(),
                is_default: false,
            },
            InstanceSummary {
                symbol: "SOLUSDT".into(),
                environment: "testnet".into(),
                is_default: false,
            },
        ];

        let vm = instances(&state);

        assert_eq!(vm.environment, "testnet");
        assert_eq!(vm.current_symbol.as_deref(), Some("ETHUSDT"));
        assert_eq!(vm.default_symbol.as_deref(), Some("BTCUSDT"));
        assert_eq!(vm.items[0].symbol, "ETHUSDT");
        assert!(vm.items[0].is_current);
        assert!(vm.items[1].is_default);
    }

    #[test]
    fn grid_selector_prefers_strategy_levels_over_execution_orders() {
        let mut state = AppState::sample();
        state.execution.open_orders.clear();
        state.strategy.status_reason = Some("price drift 18.0bps".into());

        let vm = grid(&state);

        assert_eq!(vm.active_levels, 3);
        assert_eq!(vm.levels.len(), 6);
        assert_eq!(vm.levels[0].status, "OCCUPIED");
    }

    #[test]
    fn grid_selector_localizes_status_and_inventory_in_chinese() {
        let mut state = AppState::sample();
        state.ui.locale = Locale::ZhCn;
        state.strategy.status = crate::protocol::StrategyStatus::Active;
        state.runtime.position_qty = 0.250;

        let vm = grid(&state);

        assert_eq!(vm.status, "激活");
        assert_eq!(vm.inventory_bias, "多头库存");
        assert!(vm.levels.iter().any(|level| level.status == "占用"));
        assert!(vm.levels.iter().any(|level| level.status == "激活"));
    }

    #[test]
    fn strategy_orders_only_show_current_real_strategy_orders() {
        let mut live_state = strategy_order_fixture(OpenOrdersSource::StrategyMirror);
        live_state.execution.exchange_open_orders_source = OpenOrdersSource::ExchangeLive;
        live_state.execution.exchange_open_orders = live_state.execution.open_orders.clone();
        let vm = strategy_orders(&live_state);
        assert_eq!(vm.len(), 1);
        assert_eq!(vm[0].side, "SELL");
        assert_eq!(vm[0].price, "100.00");
        assert_eq!(vm[0].qty, "0.100");
        assert_eq!(vm[0].status, "NEW");

        live_state.execution.exchange_open_orders.clear();
        let vm = strategy_orders(&live_state);
        assert!(vm.is_empty());

        let mut mirror_state = strategy_order_fixture(OpenOrdersSource::StrategyMirror);
        mirror_state.execution.exchange_open_orders_source = OpenOrdersSource::StrategyMirror;
        mirror_state.execution.exchange_open_orders = mirror_state.execution.open_orders.clone();
        let vm = strategy_orders(&mirror_state);
        assert!(vm.is_empty());
    }

    #[test]
    fn dashboard_exchange_orders_are_na_when_source_is_not_exchange_live() {
        let state = strategy_order_fixture(OpenOrdersSource::StrategyMirror);

        let vm = dashboard(&state);

        assert_eq!(vm.exchange_orders, None);
    }

    #[test]
    fn dashboard_exchange_orders_use_real_exchange_count_instead_of_strategy_mirror_count() {
        let mut state = strategy_order_fixture(OpenOrdersSource::StrategyMirror);
        state.execution.exchange_open_orders_source = OpenOrdersSource::ExchangeLive;
        state.execution.exchange_open_orders = vec![crate::protocol::OpenOrder {
            order_id: "real_ord_01".into(),
            client_order_id: "real_grid_sell_01".into(),
            side: "sell".into(),
            price: 100.0,
            qty: 0.1,
            filled_qty: 0.0,
            status: "NEW".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }];

        let vm = dashboard(&state);

        assert_eq!(vm.exchange_orders, Some(1));
    }

    #[test]
    fn risk_action_hint_changes_with_locale() {
        assert_eq!(
            risk_action_hint(Locale::EnUs, "STOP_LOSS_TRIGGERED"),
            "Reduce exposure before resuming the grid."
        );
        assert_eq!(
            risk_action_hint(Locale::ZhCn, "STOP_LOSS_TRIGGERED"),
            "恢复网格前先降低风险敞口。"
        );
    }

    fn strategy_order_fixture(source: OpenOrdersSource) -> AppState {
        let mut state = AppState::sample();
        state.execution.open_orders_source = source;
        state.strategy.levels = vec![
            GridLevel {
                level_id: "buy_01".into(),
                side: GridSide::Buy,
                price: 90.0,
                quantity: 0.100,
                state: GridLevelState::Occupied,
                client_order_id: None,
                order_id: None,
            },
            GridLevel {
                level_id: "sell_01".into(),
                side: GridSide::Sell,
                price: 100.0,
                quantity: 0.100,
                state: GridLevelState::Active,
                client_order_id: Some("grid_sell_01".into()),
                order_id: Some("ord_1002".into()),
            },
            GridLevel {
                level_id: "sell_02".into(),
                side: GridSide::Sell,
                price: 110.0,
                quantity: 0.100,
                state: GridLevelState::Active,
                client_order_id: Some("grid_sell_02".into()),
                order_id: Some("ord_1003".into()),
            },
        ];
        state.execution.open_orders = vec![crate::protocol::OpenOrder {
            order_id: "ord_1002".into(),
            client_order_id: "grid_sell_01".into(),
            side: "sell".into(),
            price: 100.0,
            qty: 0.100,
            filled_qty: 0.0,
            status: "NEW".into(),
            created_at: "2025-01-01T00:00:00Z".into(),
            updated_at: "2025-01-01T00:00:00Z".into(),
        }];
        state
            .execution
            .open_orders
            .push(crate::protocol::OpenOrder {
                order_id: "manual_ord_01".into(),
                client_order_id: "manual_sell_01".into(),
                side: "sell".into(),
                price: 105.0,
                qty: 0.100,
                filled_qty: 0.0,
                status: "NEW".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: "2025-01-01T00:00:00Z".into(),
            });
        state
    }
}
