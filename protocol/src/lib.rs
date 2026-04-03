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
pub struct TrackListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
    #[serde(default)]
    pub statistics: TrackListStatisticsView,
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
pub struct TrackListStatisticsView {
    pub total_pnl: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBadgeView {
    pub state: ExecutionStateView,
    pub execution_status: ExecutionStatusView,
    pub active_slot_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    #[serde(default)]
    pub statistics: GridStatisticsView,
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
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridMarketView {
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridPositionView {
    pub current_exposure: f64,
    #[serde(default)]
    pub target_exposure: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GridStatisticsView {
    pub total_pnl: f64,
    pub realized_pnl: f64,
    #[serde(default)]
    pub max_inventory_gap_abs: f64,
    #[serde(default)]
    pub max_gap_age_ms: i64,
    #[serde(default)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackStreamEvent {
    pub track_id: String,
    pub payload: TrackStreamPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrackStreamPayload {
    TrackListItemChanged { item: TrackListItemView },
    TrackDetailChanged { detail: Box<TrackDetailView> },
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
        GridCommandType, TrackCommandAccepted, TrackCommandRequest, TrackDiagnosticsView,
        TrackListResponse, TrackStreamEvent, TrackStreamPayload,
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
                        "statistics":{"total_pnl":1245.3}
                    }
                ]
            }"#,
        )
        .unwrap();
        let serialized = serde_json::to_value(&response).unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(
            serialized["items"][0]["statistics"]["total_pnl"].as_f64(),
            Some(1245.3)
        );
        assert_eq!(
            serialized["items"][0]["statistics"].get("realized_pnl"),
            None
        );
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
        let event: TrackStreamEvent = serde_json::from_str(
            r#"{
                "track_id":"btc-core",
                "payload":{
                    "type":"track_detail_changed",
                    "detail":{
                        "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                        "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"reference_price":64000.0},
                        "strategy":{"lower_price":60000.0,"upper_price":68000.0,"shape_family":"linear","out_of_band_policy":"freeze"},
                        "market":{"mark_price":64123.4,"index_price":64120.1},
                        "position":{"current_exposure":0.5,"target_exposure":0.75},
                        "statistics":{"total_pnl":1245.3,"realized_pnl":980.1,"max_inventory_gap_abs":0.0,"max_gap_age_ms":0,"stats_started_at":null},
                        "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                        "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                        "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(event.track_id, "btc-core");
        match event.payload {
            TrackStreamPayload::TrackDetailChanged { detail } => {
                assert_eq!(detail.identity.id, "btc-core");
            }
            _ => panic!("unexpected payload variant"),
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
