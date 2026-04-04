use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackListResponse {
    pub items: Vec<TrackListItemView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
    pub pnl: TrackListPnlView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentView {
    pub venue: String,
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridLifecycleView {
    pub status: GridStatus,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExposureSummaryView {
    pub current: f64,
    #[serde(default)]
    pub target: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackListPnlView {
    pub total_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBadgeView {
    pub state: ExecutionStateView,
    pub execution_status: ExecutionStatusView,
    pub active_slot_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    pub pnl: TrackPnlView,
    pub execution_stats: TrackExecutionStatsView,
    pub execution: GridExecutionView,
    pub activity: Vec<GridActivityItemView>,
    pub available_commands: Vec<GridCommandView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridIdentityView {
    pub id: String,
    pub instrument: InstrumentView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridStatusPanelView {
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridStrategyView {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridMarketView {
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GridPositionView {
    pub current_exposure: f64,
    #[serde(default)]
    pub target_exposure: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackPnlView {
    pub total_pnl: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackExecutionStatsView {
    pub max_inventory_gap_abs: f64,
    pub max_gap_age_ms: i64,
    pub stats_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridExecutionView {
    pub state: ExecutionStateView,
    #[serde(default)]
    pub execution_status: ExecutionStatusView,
    #[serde(default)]
    pub attention_reasons: Vec<String>,
    #[serde(default)]
    pub inventory_gap: f64,
    #[serde(default)]
    pub gap_age_ms: i64,
    #[serde(default)]
    pub active_slot_count: u32,
    #[serde(default)]
    pub slots: Vec<ExecutionSlotView>,
    #[serde(default)]
    pub replacement_gate: Option<ReplacementGateView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSlotView {
    pub label: String,
    pub phase: ExecutionSlotPhaseView,
    pub intent: ExecutionIntentView,
    #[serde(default)]
    pub order: Option<ExecutionSlotOrderView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionSlotOrderView {
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatusView {
    #[default]
    Normal,
    AttentionRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSlotPhaseView {
    Opening,
    Working,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionIntentView {
    IncreaseInventory,
    DecreaseInventory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplacementGateView {
    RoundedMatch,
    ImprovementBelowThreshold {
        improvement_bps: f64,
        threshold_bps: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridActivityItemView {
    pub ts: String,
    pub message: String,
    pub level: ActivityLevelView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDiagnosticsView {
    pub items: Vec<TrackDiagnosticItemView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDiagnosticItemView {
    pub ts: String,
    pub message: String,
    pub level: ActivityLevelView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevelView {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridCommandView {
    pub command: GridCommandType,
    pub enabled: bool,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackCommandRequest {
    pub command: GridCommandType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackCommandAccepted {
    pub track_id: String,
    pub command: GridCommandType,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridCommandType {
    Pause,
    Resume,
    Terminate,
    Flatten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStateView {
    Open,
    Paused,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSignalView {
    #[default]
    Normal,
    Attention,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AccountSummaryView {
    #[serde(default)]
    pub equity: Option<f64>,
    #[serde(default)]
    pub available: Option<f64>,
    #[serde(default)]
    pub unrealized_pnl: Option<f64>,
    #[serde(default)]
    pub day_change_pct: Option<f64>,
    #[serde(default)]
    pub risk_signal: RiskSignalView,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub day_base_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    TrackListItemChanged {
        track_id: String,
        item: TrackListItemView,
    },
    TrackDetailChanged {
        track_id: String,
        detail: Box<TrackDetailView>,
    },
    AccountSummaryChanged {
        summary: AccountSummaryView,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeFamily {
    Linear,
    Convex,
    Concave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutOfBandPolicy {
    Freeze,
    ReduceOnly,
    Terminate,
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl fmt::Display for GridStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::WaitingMarketData => "waiting",
            Self::Active => "active",
            Self::Frozen => "frozen",
            Self::ReducingOnly => "reducing_only",
            Self::Holding => "holding",
            Self::Terminated => "terminated",
            Self::Paused => "paused",
        };

        f.write_str(value)
    }
}

impl fmt::Display for ShapeFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Linear => "linear",
            Self::Convex => "convex",
            Self::Concave => "concave",
        };

        f.write_str(value)
    }
}

impl fmt::Display for OutOfBandPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Freeze => "freeze",
            Self::ReduceOnly => "reduce_only",
            Self::Terminate => "terminate",
            Self::Hold => "hold",
        };

        f.write_str(value)
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        };

        f.write_str(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountSummaryView, GridCommandType, RiskSignalView, StreamEvent, TrackCommandAccepted,
        TrackCommandRequest, TrackDetailView, TrackDiagnosticsView, TrackListResponse,
    };

    #[test]
    fn deserializes_track_list_response() {
        let response: TrackListResponse = serde_json::from_str(
            r#"{
                "items":[
                    {
                        "id":"btc-core",
                        "instrument":{"venue":"binance_futures","symbol":"BTCUSDT"},
                        "lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},
                        "reference_price":64123.4,
                        "exposure":{"current":0.5,"target":0.75},
                        "execution":{"state":"open","execution_status":"normal","active_slot_count":1},
                        "pnl":{"total_pnl":1245.3}
                    }
                ]
            }"#,
        )
        .unwrap();
        let serialized = serde_json::to_value(&response).unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(serialized["items"][0]["pnl"]["total_pnl"].as_f64(), Some(1245.3));
        assert_eq!(serialized["items"][0]["pnl"].get("realized_pnl"), None);
    }

    #[test]
    fn deserializes_track_list_response_with_pnl_field() {
        let response: TrackListResponse = serde_json::from_str(
            r#"{
                "items":[
                    {
                        "id":"btc-core",
                        "instrument":{"venue":"binance_futures","symbol":"BTCUSDT"},
                        "lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},
                        "reference_price":64123.4,
                        "exposure":{"current":0.5,"target":0.75},
                        "execution":{"state":"open","execution_status":"normal","active_slot_count":1},
                        "pnl":{"total_pnl":1245.3}
                    }
                ]
            }"#,
        )
        .unwrap();

        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(serialized["items"][0]["pnl"]["total_pnl"].as_f64(), Some(1245.3));
    }

    #[test]
    fn deserializes_track_command_accepted_with_track_id() {
        let response: TrackCommandAccepted =
            serde_json::from_str(r#"{"track_id":"btc-core","command":"pause","accepted":true}"#)
                .unwrap();

        assert_eq!(response.track_id, "btc-core");
        assert_eq!(response.command, GridCommandType::Pause);
        assert!(response.accepted);
    }

    #[test]
    fn deserializes_track_stream_detail_changed_with_track_id() {
        let event: StreamEvent = serde_json::from_str(
            r#"{
                "type":"track_detail_changed",
                "track_id":"btc-core",
                "detail":{
                    "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                    "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"reference_price":64000.0},
                    "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze"},
                    "market":{"mark_price":64123.4,"index_price":64120.1},
                    "position":{"current_exposure":0.5,"target_exposure":0.75},
                    "pnl":{"total_pnl":1245.3,"realized_pnl":980.1,"unrealized_pnl":265.2},
                    "execution_stats":{"max_inventory_gap_abs":0.0,"max_gap_age_ms":0,"stats_started_at":null},
                    "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                    "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                    "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
                }
            }"#,
        )
        .unwrap();

        match event {
            StreamEvent::TrackDetailChanged { track_id, detail } => {
                assert_eq!(track_id, "btc-core");
                let detail_json = serde_json::to_value(&detail).unwrap();
                assert_eq!(detail.identity.id, "btc-core");
                assert_eq!(
                    detail_json["strategy"]["long_exposure_units"].as_f64(),
                    Some(8.0)
                );
                assert_eq!(
                    detail_json["strategy"]["short_exposure_units"].as_f64(),
                    Some(8.0)
                );
                assert_eq!(
                    detail_json["strategy"]["notional_per_unit"].as_f64(),
                    Some(375.0)
                );
                assert_eq!(
                    detail_json["strategy"]["min_rebalance_units"].as_f64(),
                    Some(0.5)
                );
                assert_eq!(detail_json["pnl"]["unrealized_pnl"].as_f64(), Some(265.2));
                assert_eq!(
                    detail_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
                    Some(0.0)
                );
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_track_detail_with_pnl_and_execution_stats() {
        let detail: TrackDetailView = serde_json::from_str(
            r#"{
                "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"reference_price":64000.0},
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze"},
                "market":{"mark_price":64123.4,"index_price":64120.1},
                "position":{"current_exposure":0.5,"target_exposure":0.75},
                "pnl":{"total_pnl":1245.3,"realized_pnl":980.1,"unrealized_pnl":265.2},
                "execution_stats":{"max_inventory_gap_abs":1.5,"max_gap_age_ms":120000,"stats_started_at":"2026-03-26T09:45:00Z"},
                "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
            }"#,
        )
        .unwrap();

        let detail_json = serde_json::to_value(&detail).unwrap();
        assert_eq!(detail_json["pnl"]["unrealized_pnl"].as_f64(), Some(265.2));
        assert_eq!(
            detail_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
            Some(1.5)
        );
    }

    #[test]
    fn deserializes_account_summary_changed_stream_event() {
        let event: StreamEvent = serde_json::from_str(
            r#"{
                "type":"account_summary_changed",
                "summary":{
                    "equity":12500.5,
                    "available":9800.25,
                    "unrealized_pnl":-120.75,
                    "day_change_pct":-1.35,
                    "risk_signal":"attention",
                    "reason":"day_change -1.35%",
                    "day_base_at":"2026-04-04T00:01:23Z",
                    "updated_at":"2026-04-04T01:02:03Z"
                }
            }"#,
        )
        .unwrap();

        match event {
            StreamEvent::AccountSummaryChanged { summary } => {
                assert_eq!(
                    summary,
                    AccountSummaryView {
                        equity: Some(12_500.5),
                        available: Some(9_800.25),
                        unrealized_pnl: Some(-120.75),
                        day_change_pct: Some(-1.35),
                        risk_signal: RiskSignalView::Attention,
                        reason: Some("day_change -1.35%".to_string()),
                        day_base_at: Some("2026-04-04T00:01:23Z".to_string()),
                        updated_at: Some("2026-04-04T01:02:03Z".to_string()),
                    }
                );
            }
            other => panic!("unexpected event variant: {other:?}"),
        }
    }

    #[test]
    fn deserializes_track_command_request() {
        let request: TrackCommandRequest = serde_json::from_str(r#"{"command":"pause"}"#).unwrap();

        assert_eq!(request.command, GridCommandType::Pause);
    }

    #[test]
    fn deserializes_track_diagnostics_response() {
        let payload: TrackDiagnosticsView = serde_json::from_str(
            r#"{
                "items":[
                    {
                        "ts":"2026-04-03T02:26:47Z",
                        "message":"target exposure -3.9534 -> -3.7500",
                        "level":"info"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(payload.items.len(), 1);
        assert_eq!(
            payload.items[0].message,
            "target exposure -3.9534 -> -3.7500"
        );
    }
}
