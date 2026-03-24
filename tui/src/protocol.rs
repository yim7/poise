use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceStatus {
    WaitingMarketData,
    Active,
    Frozen,
    ReducingOnly,
    Holding,
    Terminated,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub last_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Client-side snapshot mirror of the HTTP DTO.
pub struct InstanceSnapshot {
    pub id: String,
    pub symbol: String,
    pub status: InstanceStatus,
    pub current_exposure: f64,
    #[serde(default)]
    pub target_exposure: Option<f64>,
    pub last_price: Option<f64>,
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
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResponse {
    pub instance_id: String,
    pub command: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridConfig {
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
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
pub enum BandBoundary {
    Below,
    Above,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainEvent {
    ExposureTargetChanged { from: f64, to: f64 },
    BandBreached { boundary: BandBoundary, price: f64 },
    BandReentered { price: f64 },
    PolicyTriggered { policy: OutOfBandPolicy },
    RiskCapApplied { intended: f64, capped: f64 },
    RiskDenied { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsEvent {
    pub instance_id: String,
    pub event: DomainEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandState {
    Unknown,
    InBand,
    BelowBand,
    AboveBand,
}

impl InstanceSnapshot {
    pub fn band_state(&self) -> BandState {
        let Some(last_price) = self.last_price else {
            return BandState::Unknown;
        };

        if last_price < self.config.lower_price - f64::EPSILON {
            BandState::BelowBand
        } else if last_price > self.config.upper_price + f64::EPSILON {
            BandState::AboveBand
        } else {
            BandState::InBand
        }
    }

    pub fn target_exposure(&self) -> Option<f64> {
        self.target_exposure
    }
}

impl fmt::Display for InstanceStatus {
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

#[cfg(test)]
mod tests {
    use super::{
        BandBoundary, BandState, CommandResponse, DomainEvent, GridConfig, InstanceSnapshot,
        InstanceStatus, InstanceSummary, OutOfBandPolicy, PendingOrder, ShapeFamily, Side, WsEvent,
    };

    #[test]
    fn deserializes_instance_summary_list() {
        let instances: Vec<InstanceSummary> =
            serde_json::from_str(include_str!("../tests/fixtures/instance_summaries.json"))
                .unwrap();

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].id, "BTCUSDT");
        assert_eq!(instances[0].status, InstanceStatus::Active);
        assert_eq!(instances[0].last_price, Some(101.25));
    }

    #[test]
    fn deserializes_instance_snapshot() {
        let snapshot: InstanceSnapshot =
            serde_json::from_str(include_str!("../tests/fixtures/instance_snapshot.json")).unwrap();

        assert_eq!(snapshot.status, InstanceStatus::Holding);
        assert_eq!(snapshot.current_exposure, 3.5);
        assert_eq!(snapshot.config.shape_family, ShapeFamily::Linear);
        assert_eq!(snapshot.band_state(), BandState::AboveBand);
        assert_eq!(snapshot.target_exposure(), Some(-4.0));
        assert!(snapshot.pending_order.is_none());
    }

    #[test]
    fn deserializes_snake_case_snapshot() {
        let snapshot: InstanceSnapshot = serde_json::from_str(
            r#"
            {
              "id": "BTCUSDT",
              "symbol": "BTCUSDT",
              "status": "holding",
              "current_exposure": 3.5,
              "last_price": 112.0,
              "config": {
                "lower_price": 90.0,
                "upper_price": 110.0,
                "long_capacity": 8.0,
                "short_capacity": 4.0,
                "capacity_notional": 375.0,
                "shape_family": "linear",
                "out_of_band_policy": "freeze"
              }
            }
            "#,
        )
        .unwrap();

        assert_eq!(snapshot.status, InstanceStatus::Holding);
        assert_eq!(snapshot.config.shape_family, ShapeFamily::Linear);
        assert_eq!(snapshot.config.out_of_band_policy, OutOfBandPolicy::Freeze);
    }

    #[test]
    fn deserializes_pending_order_side_from_snake_case_snapshot() {
        let snapshot: InstanceSnapshot = serde_json::from_str(
            r#"
            {
              "id": "BTCUSDT",
              "symbol": "BTCUSDT",
              "status": "active",
              "current_exposure": 0.0,
              "target_exposure": 4.0,
              "last_price": 95.0,
              "pending_order": {
                "symbol": "BTCUSDT",
                "order_id": "order-1",
                "client_order_id": "client-1",
                "side": "buy",
                "price": 94.5,
                "quantity": 0.25,
                "status": "NEW"
              },
              "config": {
                "lower_price": 90.0,
                "upper_price": 110.0,
                "long_capacity": 8.0,
                "short_capacity": 8.0,
                "capacity_notional": 375.0,
                "shape_family": "linear",
                "out_of_band_policy": "freeze"
              }
            }
            "#,
        )
        .unwrap();

        assert_eq!(snapshot.pending_order.unwrap().side, Side::Buy);
    }

    #[test]
    fn deserializes_command_response() {
        let response: CommandResponse =
            serde_json::from_str(include_str!("../tests/fixtures/command_response.json")).unwrap();

        assert_eq!(response.instance_id, "BTCUSDT");
        assert!(response.accepted);
    }

    #[test]
    fn deserializes_ws_event() {
        let event: WsEvent =
            serde_json::from_str(include_str!("../tests/fixtures/ws_event.json")).unwrap();

        assert_eq!(
            event,
            WsEvent {
                instance_id: "BTCUSDT".into(),
                event: DomainEvent::ExposureTargetChanged { from: 0.0, to: 4.0 },
            }
        );
    }

    #[test]
    fn reports_band_state_boundaries() {
        let snapshot = InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            current_exposure: 0.0,
            target_exposure: Some(4.0),
            last_price: Some(85.0),
            pending_order: Some(PendingOrder {
                symbol: "BTCUSDT".into(),
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 90.0,
                quantity: 0.5,
                status: "NEW".into(),
            }),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Convex,
                out_of_band_policy: OutOfBandPolicy::ReduceOnly,
            },
        };

        assert_eq!(snapshot.band_state(), BandState::BelowBand);

        let reentered = InstanceSnapshot {
            last_price: Some(100.0),
            ..snapshot
        };
        assert_eq!(reentered.band_state(), BandState::InBand);
    }

    #[test]
    fn target_exposure_prefers_server_value() {
        let snapshot = InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            current_exposure: 0.0,
            target_exposure: Some(1.5),
            last_price: Some(85.0),
            pending_order: None,
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        };

        assert_eq!(snapshot.target_exposure(), Some(1.5));
    }

    #[test]
    fn target_exposure_none_stays_none() {
        let snapshot = InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Paused,
            current_exposure: 0.0,
            target_exposure: None,
            last_price: Some(85.0),
            pending_order: None,
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        };

        assert_eq!(snapshot.target_exposure(), None);
    }

    #[test]
    fn formats_domain_events_for_display() {
        let event = DomainEvent::BandBreached {
            boundary: BandBoundary::Above,
            price: 120.0,
        };

        assert_eq!(event.to_string(), "band breached Above at 120.0000");
    }
}
