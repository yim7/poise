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
pub struct GridListResponse {
    pub items: Vec<GridListItemView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBadgeView {
    pub state: ExecutionStateView,
    pub pending_order_count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridExecutionView {
    pub state: ExecutionStateView,
    #[serde(default)]
    pub pending_order: Option<OrderExecutionView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderExecutionView {
    pub symbol: String,
    pub order_id: Option<String>,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridActivityItemView {
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
pub struct GridCommandRequest {
    pub command: GridCommandType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridCommandAccepted {
    pub grid_id: String,
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
pub struct GridStreamEvent {
    pub grid_id: String,
    pub payload: GridStreamPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GridStreamPayload {
    GridListItemChanged { item: GridListItemView },
    GridDetailChanged { detail: GridDetailView },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Submitting,
    New,
    PartiallyFilled,
    Filled,
    Canceling,
    Canceled,
    Rejected,
    Expired,
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

impl fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Submitting => "submitting",
            Self::New => "new",
            Self::PartiallyFilled => "partially_filled",
            Self::Filled => "filled",
            Self::Canceling => "canceling",
            Self::Canceled => "canceled",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        };

        f.write_str(value)
    }
}
