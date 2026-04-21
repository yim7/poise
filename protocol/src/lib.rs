use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStatus {
    WaitingMarketData,
    Active,
    Frozen,
    Flattening,
    ManualFlattening,
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
    pub lifecycle: TrackLifecycleView,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
    pub ledger: TrackListLedgerView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentView {
    pub venue: String,
    pub symbol: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackLifecycleView {
    pub status: TrackStatus,
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
pub struct TrackListLedgerView {
    pub total_pnl: f64,
    #[serde(default)]
    pub has_unresolved_gaps: bool,
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
    pub identity: TrackIdentityView,
    pub status: TrackStatusPanelView,
    pub strategy: TrackStrategyView,
    #[serde(default)]
    pub budget: TrackBudgetView,
    pub market: TrackMarketView,
    pub position: TrackPositionView,
    pub ledger: TrackLedgerView,
    pub execution_stats: TrackExecutionStatsView,
    pub execution: TrackExecutionView,
    pub activity: Vec<TrackActivityItemView>,
    pub available_commands: Vec<TrackCommandView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackIdentityView {
    pub id: String,
    pub instrument: InstrumentView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackStatusPanelView {
    pub lifecycle: TrackLifecycleView,
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackStrategyView {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: BandProtectionPolicy,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TrackBudgetView {
    pub max_notional: f64,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMarketView {
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyPriceStatusView {
    Live,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceExecutionBlockReasonView {
    MissingExecutionQuote,
    MarkBookDivergence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackLiveView {
    pub strategy_price: Option<f64>,
    pub strategy_price_status: StrategyPriceStatusView,
    pub mark_price: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    pub desired_exposure: Option<f64>,
    pub price_execution_block_reason: Option<PriceExecutionBlockReasonView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackPositionView {
    pub current_exposure: f64,
    #[serde(default)]
    pub desired_exposure: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackLedgerView {
    pub gross_realized_pnl: f64,
    pub net_realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub trading_fee_cumulative: f64,
    pub funding_fee_cumulative: f64,
    #[serde(default)]
    pub unresolved_gaps: Vec<TrackLedgerGapView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackLedgerGapReasonView {
    UnsupportedCommissionAsset,
    MissingCommissionAsset,
    MissingSymbol,
    UnsupportedFundingAsset,
    #[default]
    Unknown,
}

impl<'de> Deserialize<'de> for TrackLedgerGapReasonView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "unsupported_commission_asset" => Self::UnsupportedCommissionAsset,
            "missing_commission_asset" => Self::MissingCommissionAsset,
            "missing_symbol" => Self::MissingSymbol,
            "unsupported_funding_asset" => Self::UnsupportedFundingAsset,
            "unknown" => Self::Unknown,
            _ => Self::Unknown,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackLedgerGapView {
    pub gap_key: String,
    pub reason: TrackLedgerGapReasonView,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackExecutionStatsView {
    pub max_inventory_gap_abs: f64,
    pub max_gap_age_ms: i64,
    pub stats_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackExecutionView {
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
pub struct TrackActivityItemView {
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
pub struct TrackCommandView {
    pub command: TrackCommandType,
    pub enabled: bool,
    #[serde(default)]
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackCommandRequest {
    pub command: TrackCommandType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackCommandAccepted {
    pub track_id: String,
    pub command: TrackCommandType,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackCommandType {
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
    TrackLiveViewChanged {
        track_id: String,
        live: TrackLiveView,
    },
    AccountSummaryChanged {
        summary: AccountSummaryView,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeFamily {
    Linear,
    Inertial,
    Responsive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandProtectionPolicy {
    Freeze,
    Flatten {
        trigger: BandFlattenTrigger,
        recover: BandRecoverPolicy,
    },
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandFlattenTrigger {
    Immediate,
    FlattenConfirm { bps: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandRecoverPolicy {
    BackInBand,
    #[serde(alias = "price_confirm")]
    ReentryConfirm {
        bps: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl fmt::Display for TrackStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::WaitingMarketData => "waiting",
            Self::Active => "active",
            Self::Frozen => "frozen",
            Self::Flattening => "flattening",
            Self::ManualFlattening => "manual_flattening",
            Self::Terminated => "terminated",
            Self::Paused => "paused",
        };

        f.write_str(value)
    }
}

impl fmt::Display for StrategyPriceStatusView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Live => "live",
            Self::Stale => "stale",
        };

        f.write_str(value)
    }
}

impl fmt::Display for ShapeFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Linear => "linear",
            Self::Inertial => "inertial",
            Self::Responsive => "responsive",
        };

        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BandProtectionPolicySerde {
    Freeze {},
    Flatten {
        trigger: BandFlattenTrigger,
        recover: BandRecoverPolicy,
    },
    Terminate {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyBandProtectionPolicySerde {
    Freeze {},
    Flatten {
        trigger_bps: u32,
        recover: BandRecoverPolicy,
    },
    Terminate {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyBandProtectionPolicyKind {
    Freeze,
    Flatten,
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum BandProtectionPolicyDeserialize {
    Canonical(BandProtectionPolicySerde),
    LegacyCanonical(LegacyBandProtectionPolicySerde),
    Legacy(LegacyBandProtectionPolicyKind),
}

impl BandProtectionPolicy {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Freeze => "freeze",
            Self::Flatten { .. } => "flatten",
            Self::Terminate => "terminate",
        }
    }

    fn canonical(self) -> BandProtectionPolicySerde {
        match self {
            Self::Freeze => BandProtectionPolicySerde::Freeze {},
            Self::Flatten { trigger, recover } => {
                BandProtectionPolicySerde::Flatten { trigger, recover }
            }
            Self::Terminate => BandProtectionPolicySerde::Terminate {},
        }
    }

    fn legacy_default(value: LegacyBandProtectionPolicyKind) -> Self {
        match value {
            LegacyBandProtectionPolicyKind::Freeze => Self::Freeze,
            LegacyBandProtectionPolicyKind::Flatten => Self::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
            },
            LegacyBandProtectionPolicyKind::Terminate => Self::Terminate,
        }
    }
}

impl From<BandProtectionPolicySerde> for BandProtectionPolicy {
    fn from(value: BandProtectionPolicySerde) -> Self {
        match value {
            BandProtectionPolicySerde::Freeze {} => Self::Freeze,
            BandProtectionPolicySerde::Flatten { trigger, recover } => {
                Self::Flatten { trigger, recover }
            }
            BandProtectionPolicySerde::Terminate {} => Self::Terminate,
        }
    }
}

impl From<LegacyBandProtectionPolicySerde> for BandProtectionPolicy {
    fn from(value: LegacyBandProtectionPolicySerde) -> Self {
        match value {
            LegacyBandProtectionPolicySerde::Freeze {} => Self::Freeze,
            LegacyBandProtectionPolicySerde::Flatten {
                trigger_bps,
                recover,
            } => Self::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: trigger_bps },
                recover,
            },
            LegacyBandProtectionPolicySerde::Terminate {} => Self::Terminate,
        }
    }
}

impl Serialize for BandProtectionPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.canonical().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BandProtectionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(
            match BandProtectionPolicyDeserialize::deserialize(deserializer)? {
                BandProtectionPolicyDeserialize::Canonical(value) => value.into(),
                BandProtectionPolicyDeserialize::LegacyCanonical(value) => value.into(),
                BandProtectionPolicyDeserialize::Legacy(value) => Self::legacy_default(value),
            },
        )
    }
}

impl fmt::Display for BandProtectionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind_str())
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
        AccountSummaryView, BandFlattenTrigger, BandProtectionPolicy, BandRecoverPolicy,
        RiskSignalView, ShapeFamily, StrategyPriceStatusView, StreamEvent, TrackCommandAccepted,
        TrackCommandRequest, TrackCommandType, TrackDetailView, TrackDiagnosticsView,
        TrackLedgerGapReasonView, TrackListResponse, TrackStatus,
    };

    #[test]
    fn shape_family_serializes_new_behavior_names() {
        let payload = serde_json::to_string(&ShapeFamily::Responsive).unwrap();
        assert_eq!(payload, "\"responsive\"");
        assert_eq!(ShapeFamily::Inertial.to_string(), "inertial");
    }

    #[test]
    fn shape_family_rejects_legacy_geometry_names() {
        assert!(serde_json::from_str::<ShapeFamily>("\"concave\"").is_err());
        assert!(serde_json::from_str::<ShapeFamily>("\"convex\"").is_err());
    }

    #[test]
    fn band_protection_policy_serializes_flatten_as_object() {
        let payload = serde_json::to_value(BandProtectionPolicy::Flatten {
            trigger: BandFlattenTrigger::FlattenConfirm { bps: 500 },
            recover: BandRecoverPolicy::ReentryConfirm { bps: 500 },
        })
        .unwrap();

        assert_eq!(
            payload,
            serde_json::json!({
                "flatten": {
                    "trigger": {
                        "flatten_confirm": { "bps": 500 }
                    },
                    "recover": {
                        "reentry_confirm": { "bps": 500 }
                    }
                }
            })
        );
    }

    #[test]
    fn track_status_displays_manual_flattening() {
        let status: TrackStatus = serde_json::from_str("\"manual_flattening\"").unwrap();

        assert_eq!(status.to_string(), "manual_flattening");
    }

    #[test]
    fn deserializes_track_list_response() {
        let response: TrackListResponse = serde_json::from_str(
            r#"{
                "items":[
                    {
                        "id":"btc-core",
                        "instrument":{"venue":"binance_futures","symbol":"BTCUSDT"},
                        "lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},
                        "strategy_price":64123.4,
                        "strategy_price_status":"live",
                        "exposure":{"current":0.5,"target":0.75},
                        "execution":{"state":"open","execution_status":"normal","active_slot_count":1},
                        "ledger":{"total_pnl":1229.0,"has_unresolved_gaps":false}
                    }
                ]
            }"#,
        )
        .unwrap();
        let serialized = serde_json::to_value(&response).unwrap();

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].id, "btc-core");
        assert_eq!(
            serialized["items"][0]["ledger"]["total_pnl"].as_f64(),
            Some(1229.0)
        );
        assert_eq!(
            serialized["items"][0]["ledger"]["has_unresolved_gaps"].as_bool(),
            Some(false)
        );
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
                        "strategy_price":64123.4,
                        "strategy_price_status":"live",
                        "exposure":{"current":0.5,"target":0.75},
                        "execution":{"state":"open","execution_status":"normal","active_slot_count":1},
                        "ledger":{"total_pnl":1229.0,"has_unresolved_gaps":false}
                    }
                ]
            }"#,
        )
        .unwrap();

        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(
            serialized["items"][0]["ledger"]["total_pnl"].as_f64(),
            Some(1229.0)
        );
    }

    #[test]
    fn detail_view_serializes_strategy_price_and_quote_fields() {
        let detail: TrackDetailView = serde_json::from_str(
            r#"{
                "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                "status":{
                    "lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},
                    "strategy_price":64000.0,
                    "strategy_price_status":"live"
                },
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":{"freeze":{}}},
                "budget":{"max_notional":3000.0,"daily_loss_limit":100.0,"total_loss_limit":300.0},
                "market":{"mark_price":64123.4,"best_bid":64120.1,"best_ask":64124.5},
                "position":{"current_exposure":0.5,"desired_exposure":0.75},
                "ledger":{"gross_realized_pnl":980.1,"net_realized_pnl":963.8,"unrealized_pnl":265.2,"total_pnl":1229.0,"trading_fee_cumulative":12.3,"funding_fee_cumulative":-4.0,"unresolved_gaps":[]},
                "execution_stats":{"max_inventory_gap_abs":1.5,"max_gap_age_ms":120000,"stats_started_at":"2026-03-26T09:45:00Z"},
                "execution":{"state":"open","execution_status":"attention_required","attention_reasons":["missing execution quote"],"inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
            }"#,
        )
        .unwrap();

        let json = serde_json::to_value(&detail).unwrap();

        assert_eq!(
            detail.status.strategy_price_status,
            StrategyPriceStatusView::Live
        );
        assert!(json["status"].get("reference_price").is_none());
        assert!(json["market"].get("index_price").is_none());
        assert_eq!(json["status"]["strategy_price"].as_f64(), Some(64000.0));
        assert_eq!(
            json["status"]["strategy_price_status"].as_str(),
            Some("live")
        );
        assert_eq!(json["market"]["mark_price"].as_f64(), Some(64123.4));
        assert_eq!(json["market"]["best_bid"].as_f64(), Some(64120.1));
        assert_eq!(json["market"]["best_ask"].as_f64(), Some(64124.5));
    }

    #[test]
    fn deserializes_track_command_accepted_with_track_id() {
        let response: TrackCommandAccepted =
            serde_json::from_str(r#"{"track_id":"btc-core","command":"pause","accepted":true}"#)
                .unwrap();

        assert_eq!(response.track_id, "btc-core");
        assert_eq!(response.command, TrackCommandType::Pause);
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
                    "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"strategy_price":64000.0,"strategy_price_status":"live"},
                    "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":{"freeze":{}}},
                    "market":{"mark_price":64123.4,"best_bid":64120.1,"best_ask":64124.5},
                    "position":{"current_exposure":0.5,"desired_exposure":0.75},
                    "ledger":{"gross_realized_pnl":980.1,"net_realized_pnl":963.8,"unrealized_pnl":265.2,"total_pnl":1229.0,"trading_fee_cumulative":12.3,"funding_fee_cumulative":-4.0,"unresolved_gaps":[]},
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
                assert_eq!(
                    detail_json["ledger"]["unrealized_pnl"].as_f64(),
                    Some(265.2)
                );
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
                "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"strategy_price":64000.0,"strategy_price_status":"live"},
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":{"freeze":{}}},
                "market":{"mark_price":64123.4,"best_bid":64120.1,"best_ask":64124.5},
                "position":{"current_exposure":0.5,"desired_exposure":0.75},
                "ledger":{"gross_realized_pnl":980.1,"net_realized_pnl":963.8,"unrealized_pnl":265.2,"total_pnl":1229.0,"trading_fee_cumulative":12.3,"funding_fee_cumulative":-4.0,"unresolved_gaps":[{"gap_key":"gap-1","reason":"unsupported_commission_asset","observed_at":"2026-03-31T11:11:11Z"}]},
                "execution_stats":{"max_inventory_gap_abs":1.5,"max_gap_age_ms":120000,"stats_started_at":"2026-03-26T09:45:00Z"},
                "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
            }"#,
        )
        .unwrap();

        let detail_json = serde_json::to_value(&detail).unwrap();
        assert_eq!(
            detail_json["ledger"]["unrealized_pnl"].as_f64(),
            Some(265.2)
        );
        assert_eq!(
            detail.ledger.unresolved_gaps[0].reason,
            TrackLedgerGapReasonView::UnsupportedCommissionAsset
        );
        assert_eq!(
            detail_json["execution_stats"]["max_inventory_gap_abs"].as_f64(),
            Some(1.5)
        );
    }

    #[test]
    fn deserializes_unknown_track_ledger_gap_reason_as_unknown() {
        let detail: TrackDetailView = serde_json::from_str(
            r#"{
                "identity":{"id":"btc-core","instrument":{"venue":"binance_futures","symbol":"BTCUSDT"}},
                "status":{"lifecycle":{"status":"active","updated_at":"2026-03-31T12:34:56Z"},"strategy_price":64000.0,"strategy_price_status":"live"},
                "strategy":{"lower_price":60000.0,"upper_price":68000.0,"long_exposure_units":8.0,"short_exposure_units":8.0,"notional_per_unit":375.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":{"freeze":{}}},
                "market":{"mark_price":64123.4,"best_bid":64120.1,"best_ask":64124.5},
                "position":{"current_exposure":0.5,"desired_exposure":0.75},
                "ledger":{"gross_realized_pnl":980.1,"net_realized_pnl":963.8,"unrealized_pnl":265.2,"total_pnl":1229.0,"trading_fee_cumulative":12.3,"funding_fee_cumulative":-4.0,"unresolved_gaps":[{"gap_key":"gap-1","reason":"future_gap_reason","observed_at":"2026-03-31T11:11:11Z"}]},
                "execution_stats":{"max_inventory_gap_abs":1.5,"max_gap_age_ms":120000,"stats_started_at":"2026-03-26T09:45:00Z"},
                "execution":{"state":"open","execution_status":"normal","inventory_gap":0.0,"gap_age_ms":0,"active_slot_count":0,"slots":[]},
                "activity":[{"ts":"2026-03-31T12:34:56Z","message":"Track activated","level":"info"}],
                "available_commands":[{"command":"pause","enabled":true,"disabled_reason":null}]
            }"#,
        )
        .unwrap();

        assert_eq!(
            detail.ledger.unresolved_gaps[0].reason,
            TrackLedgerGapReasonView::Unknown
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

        assert_eq!(request.command, TrackCommandType::Pause);
    }

    #[test]
    fn deserializes_track_diagnostics_response() {
        let payload: TrackDiagnosticsView = serde_json::from_str(
            r#"{
                "items":[
                    {
                        "ts":"2026-04-03T02:26:47Z",
                        "message":"desired exposure -3.9534 -> -3.7500",
                        "level":"info"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(payload.items.len(), 1);
        assert_eq!(
            payload.items[0].message,
            "desired exposure -3.9534 -> -3.7500"
        );
    }
}
