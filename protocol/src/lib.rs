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
pub struct GridSummary {
    pub id: String,
    pub symbol: String,
    pub status: GridStatus,
    pub reference_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub id: String,
    pub symbol: String,
    pub status: GridStatus,
    pub current_exposure: f64,
    #[serde(default)]
    pub target_exposure: Option<f64>,
    pub reference_price: Option<f64>,
    #[serde(default)]
    pub pending_order: Option<PendingOrder>,
    pub config: GridConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOrder {
    pub symbol: String,
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub status: OrderStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub grid_id: String,
    pub command: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub shape_family: ShapeFamily,
    pub out_of_band_policy: OutOfBandPolicy,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandBoundary {
    Below,
    Above,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainEvent {
    SnapshotUpdated,
    ExposureTargetChanged { from: f64, to: f64 },
    BandBreached { boundary: BandBoundary, price: f64 },
    BandReentered { price: f64 },
    PolicyTriggered { policy: OutOfBandPolicy },
    RiskCapApplied { intended: f64, capped: f64 },
    RiskDenied { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsEvent {
    pub grid_id: String,
    pub event: DomainEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandState {
    Unknown,
    InBand,
    BelowBand,
    AboveBand,
}

impl GridSnapshot {
    pub fn band_state(&self) -> BandState {
        let Some(reference_price) = self.reference_price else {
            return BandState::Unknown;
        };

        if reference_price < self.config.lower_price - f64::EPSILON {
            BandState::BelowBand
        } else if reference_price > self.config.upper_price + f64::EPSILON {
            BandState::AboveBand
        } else {
            BandState::InBand
        }
    }

    pub fn target_exposure(&self) -> Option<f64> {
        self.target_exposure
    }
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

impl fmt::Display for BandState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Unknown => "unknown",
            Self::InBand => "in_band",
            Self::BelowBand => "below_band",
            Self::AboveBand => "above_band",
        };

        f.write_str(value)
    }
}

impl fmt::Display for DomainEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotUpdated => write!(f, "snapshot updated"),
            Self::ExposureTargetChanged { from, to } => {
                write!(f, "target exposure {:.4} -> {:.4}", from, to)
            }
            Self::BandBreached { boundary, price } => {
                write!(f, "band breached {:?} at {:.4}", boundary, price)
            }
            Self::BandReentered { price } => write!(f, "band reentered at {:.4}", price),
            Self::PolicyTriggered { policy } => write!(f, "policy triggered: {}", policy),
            Self::RiskCapApplied { intended, capped } => {
                write!(f, "risk cap {:.4} -> {:.4}", intended, capped)
            }
            Self::RiskDenied { reason } => write!(f, "risk denied: {}", reason),
        }
    }
}
