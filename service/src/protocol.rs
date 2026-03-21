use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: &str = "v1alpha1";
pub const DEFAULT_INSTANCE_ID: &str = "local";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpSuccessEnvelope<T> {
    pub version: String,
    pub status: String,
    pub data: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpErrorDetail {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpErrorEnvelope {
    pub version: String,
    pub status: String,
    pub error: HttpErrorDetail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    Pause,
    Resume,
    CancelAll,
    FlattenNow,
    ShutdownAfterFlatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Pending,
    Accepted,
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Ok,
    Watch,
    Warning,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    Active,
    Occupied,
    PendingRebuild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridLevelState {
    Active,
    Occupied,
    PendingRebuild,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub spacing_bps: f64,
    pub levels_per_side: u32,
    pub quantity_per_level: f64,
    pub max_position_qty: f64,
    pub rebuild_threshold_bps: f64,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            spacing_bps: 35.0,
            levels_per_side: 3,
            quantity_per_level: 0.1,
            max_position_qty: 0.3,
            rebuild_threshold_bps: 120.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridLevel {
    pub level_id: String,
    pub side: GridSide,
    pub price: f64,
    pub quantity: f64,
    pub state: GridLevelState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyState {
    #[serde(default)]
    pub config: GridConfig,
    pub status: StrategyStatus,
    pub center_price: f64,
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub rebuild_reference_price: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_rebuild_reason: Option<String>,
    #[serde(default)]
    pub levels: Vec<GridLevel>,
}

impl Default for StrategyState {
    fn default() -> Self {
        Self {
            config: GridConfig::default(),
            status: StrategyStatus::Active,
            center_price: 0.0,
            lower_bound: 0.0,
            upper_bound: 0.0,
            rebuild_reference_price: 0.0,
            pending_rebuild_reason: None,
            levels: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionState {
    pub http_available: bool,
    pub ws_connected: bool,
    #[serde(default)]
    pub user_stream_connected: Option<bool>,
    pub latency_ms: Option<u32>,
    pub last_heartbeat_at: String,
    pub reconnect_backoff_ms: u64,
    pub stale_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub symbol: String,
    pub env: String,
    pub session_state: String,
    pub strategy_state: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub position_qty: f64,
    pub position_avg_price: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub order_id: String,
    pub client_order_id: String,
    pub side: String,
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenOrdersSource {
    ExchangeLive,
    StrategyMirror,
    Unavailable,
}

impl Default for OpenOrdersSource {
    fn default() -> Self {
        Self::StrategyMirror
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentFill {
    pub trade_id: String,
    pub order_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    pub side: String,
    pub price: f64,
    pub qty: f64,
    pub fee: f64,
    pub realized_pnl: f64,
    pub event_time: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCommand {
    pub command_id: String,
    pub command: CommandType,
    pub status: CommandStatus,
    pub requested_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandLinks {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_order_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trade_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRecord {
    pub command_id: String,
    pub command: CommandType,
    pub status: CommandStatus,
    pub summary: String,
    pub requested_at: String,
    pub accepted_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(flatten)]
    pub links: CommandLinks,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionState {
    pub open_orders: Vec<OpenOrder>,
    #[serde(default)]
    pub open_orders_source: OpenOrdersSource,
    #[serde(default)]
    pub exchange_open_orders: Vec<OpenOrder>,
    #[serde(default = "default_exchange_open_orders_source")]
    pub exchange_open_orders_source: OpenOrdersSource,
    pub recent_fills: Vec<RecentFill>,
    pub pending_commands: Vec<PendingCommand>,
    pub last_command_ack: Option<String>,
    pub last_command_ack_event: Option<CommandAck>,
    #[serde(default)]
    pub recent_commands: Vec<CommandRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    pub current_notional: f64,
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub stop_loss_pct: f64,
    pub risk_level: RiskLevel,
    #[serde(default)]
    pub max_position_exceeded: bool,
    #[serde(default)]
    pub stop_loss_triggered: bool,
    #[serde(default)]
    pub daily_loss_breached: bool,
    pub breaker_engaged: bool,
    pub unacked_alerts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub connection: ConnectionState,
    pub runtime: RuntimeState,
    pub execution: ExecutionState,
    pub risk: RiskState,
    #[serde(default)]
    pub strategy: StrategyState,
}

fn default_exchange_open_orders_source() -> OpenOrdersSource {
    OpenOrdersSource::Unavailable
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeQueryResult {
    pub instance_id: String,
    pub snapshot: RuntimeSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pagination {
    pub page: usize,
    pub per_page: usize,
    pub total_items: usize,
    pub total_pages: usize,
    pub has_next: bool,
    pub has_prev: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryCollection<T, F> {
    pub instance_id: String,
    pub items: Vec<T>,
    pub pagination: Pagination,
    pub filters: F,
    pub sort: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrdersFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillsFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertsFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandsFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

pub type OrdersQueryResult = QueryCollection<OpenOrder, OrdersFilters>;
pub type FillsQueryResult = QueryCollection<RecentFill, FillsFilters>;
pub type AlertsQueryResult = QueryCollection<AlertRecord, AlertsFilters>;
pub type CommandsQueryResult = QueryCollection<CommandRecord, CommandsFilters>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub category: String,
    pub severity: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneCapabilities {
    pub instance_id: String,
    pub deployment: DeploymentDescriptor,
    pub auth: AuthDescriptor,
    pub endpoint_groups: Vec<EndpointGroup>,
    pub websocket: WebSocketDescriptor,
    pub minimal_web_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentDescriptor {
    pub mode: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDescriptor {
    pub mode: String,
    pub http: HttpAuthDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpAuthDescriptor {
    pub header: String,
    pub query_param: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointGroup {
    pub name: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketDescriptor {
    pub path: String,
    pub subscriptions: Vec<String>,
    pub auth: WebSocketAuthDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketAuthDescriptor {
    pub query_param: String,
    pub first_message: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskEvent {
    pub severity: RiskLevel,
    pub code: String,
    pub message: String,
    pub created_at: String,
    pub acknowledged_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemEvent {
    pub level: String,
    pub source: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub version: String,
    pub command_id: String,
    pub command: CommandType,
    pub status: CommandStatus,
    pub accepted_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandAck {
    pub command_id: String,
    pub command: CommandType,
    pub status: CommandStatus,
    pub message: String,
    #[serde(flatten)]
    pub links: CommandLinks,
    pub emitted_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceUpdated {
    pub symbol: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub emitted_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerEnvelope {
    pub version: String,
    pub event_id: String,
    pub emitted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(flatten)]
    pub event: ServerEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ServerEvent {
    #[serde(rename = "runtime_snapshot")]
    RuntimeSnapshot(RuntimeSnapshot),
    #[serde(rename = "price_updated")]
    PriceUpdated(PriceUpdated),
    #[serde(rename = "command_ack")]
    CommandAck(CommandAck),
    #[serde(rename = "risk_alert")]
    RiskAlert(RiskEvent),
    #[serde(rename = "connection_changed")]
    ConnectionChanged(ConnectionState),
}

impl RuntimeSnapshot {
    pub fn empty_bootstrap() -> Self {
        Self {
            connection: ConnectionState {
                http_available: true,
                ws_connected: false,
                user_stream_connected: None,
                latency_ms: None,
                last_heartbeat_at: String::new(),
                reconnect_backoff_ms: 0,
                stale_age_ms: 0,
            },
            runtime: RuntimeState {
                symbol: "XAUUSDT".into(),
                env: "testnet".into(),
                session_state: "regular".into(),
                strategy_state: "running".into(),
                last_price: 0.0,
                mark_price: 0.0,
                position_qty: 0.0,
                position_avg_price: 0.0,
                unrealized_pnl: 0.0,
                realized_pnl: 0.0,
            },
            execution: ExecutionState {
                open_orders: vec![],
                open_orders_source: OpenOrdersSource::StrategyMirror,
                exchange_open_orders: vec![],
                exchange_open_orders_source: OpenOrdersSource::Unavailable,
                recent_fills: vec![],
                pending_commands: vec![],
                last_command_ack: None,
                last_command_ack_event: None,
                recent_commands: vec![],
            },
            risk: RiskState {
                current_notional: 0.0,
                max_notional: 1500.0,
                daily_loss_limit: -120.0,
                stop_loss_pct: 4.0,
                risk_level: RiskLevel::Ok,
                max_position_exceeded: false,
                stop_loss_triggered: false,
                daily_loss_breached: false,
                breaker_engaged: false,
                unacked_alerts: 0,
            },
            strategy: StrategyState::default(),
        }
    }

    pub fn sample() -> Self {
        Self {
            connection: ConnectionState {
                http_available: true,
                ws_connected: false,
                user_stream_connected: None,
                latency_ms: Some(42),
                last_heartbeat_at: "2025-01-01T00:00:00Z".into(),
                reconnect_backoff_ms: 0,
                stale_age_ms: 0,
            },
            runtime: RuntimeState {
                symbol: "XAUUSDT".into(),
                env: "testnet".into(),
                session_state: "regular".into(),
                strategy_state: "running".into(),
                last_price: 2361.48,
                mark_price: 2361.55,
                position_qty: 0.25,
                position_avg_price: 2354.2,
                unrealized_pnl: 1.84,
                realized_pnl: 14.52,
            },
            execution: ExecutionState {
                open_orders: vec![
                    OpenOrder {
                        order_id: "ord_1001".into(),
                        client_order_id: "grid_buy_01".into(),
                        side: "buy".into(),
                        price: 2352.8,
                        qty: 0.10,
                        filled_qty: 0.0,
                        status: "NEW".into(),
                        created_at: "2025-01-01T00:00:00Z".into(),
                        updated_at: "2025-01-01T00:00:00Z".into(),
                    },
                    OpenOrder {
                        order_id: "ord_1002".into(),
                        client_order_id: "grid_sell_01".into(),
                        side: "sell".into(),
                        price: 2368.3,
                        qty: 0.10,
                        filled_qty: 0.0,
                        status: "NEW".into(),
                        created_at: "2025-01-01T00:00:00Z".into(),
                        updated_at: "2025-01-01T00:00:00Z".into(),
                    },
                ],
                open_orders_source: OpenOrdersSource::StrategyMirror,
                exchange_open_orders: vec![],
                exchange_open_orders_source: OpenOrdersSource::Unavailable,
                recent_fills: vec![RecentFill {
                    trade_id: "fill_9001".into(),
                    order_id: "ord_0999".into(),
                    client_order_id: Some("flatten_reduce_only_01".into()),
                    side: "buy".into(),
                    price: 2349.1,
                    qty: 0.05,
                    fee: 0.03,
                    realized_pnl: 2.51,
                    event_time: "2025-01-01T00:00:00Z".into(),
                }],
                pending_commands: vec![],
                last_command_ack: None,
                last_command_ack_event: None,
                recent_commands: vec![],
            },
            risk: RiskState {
                current_notional: 590.39,
                max_notional: 1500.0,
                daily_loss_limit: -120.0,
                stop_loss_pct: 4.0,
                risk_level: RiskLevel::Watch,
                max_position_exceeded: false,
                stop_loss_triggered: false,
                daily_loss_breached: false,
                breaker_engaged: false,
                unacked_alerts: 1,
            },
            strategy: sample_strategy(),
        }
    }
}

impl Pagination {
    pub fn new(page: usize, per_page: usize, total_items: usize) -> Self {
        let total_pages = if total_items == 0 {
            0
        } else {
            (total_items + per_page.saturating_sub(1)) / per_page
        };
        Self {
            page,
            per_page,
            total_items,
            total_pages,
            has_next: total_pages > 0 && page < total_pages,
            has_prev: page > 1 && total_pages > 0,
        }
    }
}

fn sample_strategy() -> StrategyState {
    StrategyState {
        config: GridConfig::default(),
        status: StrategyStatus::Occupied,
        center_price: 2361.48,
        lower_bound: 2336.76,
        upper_bound: 2386.20,
        rebuild_reference_price: 2361.48,
        pending_rebuild_reason: None,
        levels: vec![
            GridLevel {
                level_id: "buy_01".into(),
                side: GridSide::Buy,
                price: 2353.21,
                quantity: 0.1,
                state: GridLevelState::Occupied,
                client_order_id: Some("grid_buy_01".into()),
                order_id: Some("ord_1001".into()),
            },
            GridLevel {
                level_id: "buy_02".into(),
                side: GridSide::Buy,
                price: 2344.95,
                quantity: 0.1,
                state: GridLevelState::Occupied,
                client_order_id: None,
                order_id: None,
            },
            GridLevel {
                level_id: "buy_03".into(),
                side: GridSide::Buy,
                price: 2336.68,
                quantity: 0.1,
                state: GridLevelState::Occupied,
                client_order_id: None,
                order_id: None,
            },
            GridLevel {
                level_id: "sell_01".into(),
                side: GridSide::Sell,
                price: 2369.75,
                quantity: 0.1,
                state: GridLevelState::Active,
                client_order_id: Some("grid_sell_01".into()),
                order_id: Some("ord_1002".into()),
            },
            GridLevel {
                level_id: "sell_02".into(),
                side: GridSide::Sell,
                price: 2378.01,
                quantity: 0.1,
                state: GridLevelState::Active,
                client_order_id: None,
                order_id: None,
            },
            GridLevel {
                level_id: "sell_03".into(),
                side: GridSide::Sell,
                price: 2386.28,
                quantity: 0.1,
                state: GridLevelState::Active,
                client_order_id: None,
                order_id: None,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_snapshot_sample_matches_expected_symbol() {
        let snapshot = RuntimeSnapshot::sample();
        assert_eq!(snapshot.runtime.symbol, "XAUUSDT");
        assert_eq!(snapshot.execution.open_orders.len(), 2);
        assert_eq!(snapshot.strategy.status, StrategyStatus::Occupied);
        assert_eq!(snapshot.strategy.levels.len(), 6);
        assert_eq!(snapshot.strategy.config.levels_per_side, 3);
    }

    #[test]
    fn runtime_snapshot_sample_serializes_open_orders_source() {
        let serialized = serde_json::to_value(RuntimeSnapshot::sample()).expect("serialize sample");
        assert_eq!(
            serialized["execution"]["open_orders_source"],
            "strategy_mirror"
        );
        assert_eq!(
            serialized["execution"]["exchange_open_orders_source"],
            "unavailable"
        );
    }

    #[test]
    fn runtime_snapshot_decodes_when_open_orders_source_is_omitted() {
        let raw = json!({
            "connection": {
                "http_available": true,
                "ws_connected": false,
                "user_stream_connected": null,
                "latency_ms": 42,
                "last_heartbeat_at": "2025-01-01T00:00:00Z",
                "reconnect_backoff_ms": 0,
                "stale_age_ms": 0
            },
            "runtime": {
                "symbol": "XAUUSDT",
                "env": "testnet",
                "session_state": "regular",
                "strategy_state": "running",
                "last_price": 2361.48,
                "mark_price": 2361.55,
                "position_qty": 0.25,
                "position_avg_price": 2354.2,
                "unrealized_pnl": 1.84,
                "realized_pnl": 14.52
            },
            "execution": {
                "open_orders": [],
                "recent_fills": [],
                "pending_commands": [],
                "last_command_ack": null,
                "last_command_ack_event": null,
                "recent_commands": []
            },
            "risk": {
                "current_notional": 590.39,
                "max_notional": 1500.0,
                "daily_loss_limit": -120.0,
                "stop_loss_pct": 4.0,
                "risk_level": "watch",
                "breaker_engaged": false,
                "unacked_alerts": 1
            }
        });
        let snapshot: RuntimeSnapshot =
            serde_json::from_value(raw).expect("decode legacy snapshot");
        let serialized = serde_json::to_value(snapshot).expect("serialize decoded snapshot");

        assert_eq!(
            serialized["execution"]["open_orders_source"],
            "strategy_mirror"
        );
        assert_eq!(
            serialized["execution"]["exchange_open_orders_source"],
            "unavailable"
        );
    }

    #[test]
    fn http_success_envelope_wraps_payload() {
        let envelope = HttpSuccessEnvelope {
            version: PROTOCOL_VERSION.into(),
            status: "ok".into(),
            data: RuntimeSnapshot::sample(),
        };
        assert_eq!(envelope.status, "ok");
        assert_eq!(envelope.data.runtime.symbol, "XAUUSDT");
    }
}
