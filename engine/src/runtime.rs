use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use grid_core::risk::CapacityBudget;
use grid_core::strategy::GridConfig;
use grid_core::types::{ExchangeRules, Exposure, Side};

use crate::grid::{GridId, Instrument};
use crate::observation::OrderObservation;
use crate::ports::{ExchangeOrder, OrderReceipt, OrderRequest, OrderStatus};
use crate::snapshot::{GridRuntimeSnapshot, ObservedState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub struct PendingOrder {
    pub order_id: Option<String>,
    pub client_order_id: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub target_exposure: Exposure,
    pub status: OrderStatus,
}

impl PendingOrder {
    pub fn from_submit_request(request: &OrderRequest, target_exposure: Exposure) -> Self {
        Self {
            order_id: None,
            client_order_id: request.client_order_id.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            target_exposure,
            status: OrderStatus::Submitting,
        }
    }

    pub fn from_submit_receipt(
        request: &OrderRequest,
        target_exposure: Exposure,
        receipt: &OrderReceipt,
    ) -> Self {
        Self {
            order_id: Some(receipt.order_id.clone()),
            client_order_id: receipt.client_order_id.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            target_exposure,
            status: receipt.status,
        }
    }

    pub fn from_exchange_order(order: &ExchangeOrder, target_exposure: Exposure) -> Self {
        Self {
            order_id: Some(order.order_id.clone()),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.qty,
            target_exposure,
            status: order.status,
        }
    }

    pub fn from_order_observation(
        observation: &OrderObservation,
        target_exposure: Exposure,
    ) -> Self {
        Self {
            order_id: Some(observation.order_id.clone()),
            client_order_id: observation.client_order_id.clone(),
            side: observation.side,
            price: observation.price,
            quantity: observation.quantity,
            target_exposure,
            status: observation.status,
        }
    }

    pub fn is_submit_recovery_anchor(&self) -> bool {
        self.order_id.is_none() && self.status == OrderStatus::Submitting
    }

    pub fn target_reached(current_exposure: Exposure, target_exposure: Exposure) -> bool {
        let delta = target_exposure.0 - current_exposure.0;
        if delta.abs() <= f64::EPSILON {
            return true;
        }

        if target_exposure.0.abs() <= f64::EPSILON {
            return current_exposure.0.abs() <= f64::EPSILON;
        }

        if target_exposure.0 >= 0.0 {
            current_exposure.0 >= target_exposure.0
        } else {
            current_exposure.0 <= target_exposure.0
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitRecoveryKind {
    Submitting,
    ReceiptBacked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRecoveryAnchor {
    pub client_order_id: String,
    pub kind: SubmitRecoveryKind,
}

impl SubmitRecoveryAnchor {
    pub fn from_pending_order(pending_order: &PendingOrder) -> Option<Self> {
        if pending_order.is_submit_recovery_anchor() {
            return Some(Self {
                client_order_id: pending_order.client_order_id.clone(),
                kind: SubmitRecoveryKind::Submitting,
            });
        }

        (pending_order.order_id.is_some() && pending_order.status.keeps_pending_order()).then(|| {
            Self {
                client_order_id: pending_order.client_order_id.clone(),
                kind: SubmitRecoveryKind::ReceiptBacked,
            }
        })
    }

    pub fn matches(&self, pending_order: &PendingOrder) -> bool {
        pending_order.client_order_id == self.client_order_id
            && match self.kind {
                SubmitRecoveryKind::Submitting => pending_order.is_submit_recovery_anchor(),
                SubmitRecoveryKind::ReceiptBacked => {
                    pending_order.order_id.is_some() && pending_order.status.keeps_pending_order()
                }
            }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskState {
    pub realized_pnl_day: Option<NaiveDate>,
    pub realized_pnl_today: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone)]
pub struct GridRuntime {
    pub id: GridId,
    pub instrument: Instrument,
    pub config: GridConfig,
    pub budget: CapacityBudget,
    pub exchange_rules: ExchangeRules,
    pub status: GridStatus,
    pub current_exposure: Exposure,
    // Reconcile owns target_exposure; exchange sync/restore own observed order and risk fields.
    pub target_exposure: Option<Exposure>,
    pub pending_order: Option<PendingOrder>,
    pub risk_state: RiskState,
    pub reference_price: Option<f64>,
    pub out_of_band_since: Option<DateTime<Utc>>,
}

impl GridRuntime {
    pub fn new(
        id: GridId,
        instrument: Instrument,
        config: GridConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            budget,
            exchange_rules,
            status: GridStatus::WaitingMarketData,
            current_exposure: Exposure(0.0),
            target_exposure: None,
            pending_order: None,
            risk_state: RiskState::default(),
            reference_price: None,
            out_of_band_since: None,
        }
    }

    pub fn symbol(&self) -> &str {
        &self.instrument.symbol
    }

    pub fn snapshot(&self) -> GridRuntimeSnapshot {
        GridRuntimeSnapshot {
            grid_id: self.id.clone(),
            instrument: self.instrument.clone(),
            config: self.config.clone(),
            status: self.status.clone(),
            current_exposure: self.current_exposure.clone(),
            target_exposure: self.target_exposure.clone(),
            pending_order: self.pending_order.clone(),
            risk: self.risk_state.clone(),
            observed: ObservedState {
                reference_price: self.reference_price,
                out_of_band_since: self.out_of_band_since,
            },
        }
    }

    pub fn restore_from_snapshot(&mut self, snapshot: &GridRuntimeSnapshot) -> Result<()> {
        if self.id != snapshot.grid_id {
            anyhow::bail!(
                "snapshot grid id mismatch: runtime has `{}`, snapshot has `{}`",
                self.id.as_str(),
                snapshot.grid_id.as_str()
            );
        }
        if self.instrument != snapshot.instrument {
            anyhow::bail!(
                "snapshot instrument mismatch for `{}`: expected `{}:{}`, got `{}:{}`",
                self.id.as_str(),
                self.instrument.venue.as_str(),
                self.instrument.symbol,
                snapshot.instrument.venue.as_str(),
                snapshot.instrument.symbol
            );
        }
        if self.config != snapshot.config {
            anyhow::bail!("snapshot config mismatch for `{}`", self.id.as_str());
        }

        self.status = snapshot.status.clone();
        self.current_exposure = snapshot.current_exposure.clone();
        self.target_exposure = snapshot.target_exposure.clone();
        self.pending_order = snapshot.pending_order.clone();
        self.risk_state = snapshot.risk.clone();
        self.reference_price = snapshot.observed.reference_price;
        self.out_of_band_since = snapshot.observed.out_of_band_since;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use grid_core::types::{Exposure, Side};

    use crate::grid::{Instrument, Venue};
    use crate::observation::OrderObservation;
    use crate::ports::{OrderReceipt, OrderRequest, OrderStatus};

    use super::{PendingOrder, SubmitRecoveryAnchor, SubmitRecoveryKind};

    #[test]
    fn pending_order_builders_preserve_existing_submit_and_live_order_shapes() {
        let request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            side: Side::Buy,
            price: 94.5,
            quantity: 0.25,
            client_order_id: "client-1".into(),
        };
        let target_exposure = Exposure(6.0);

        assert_eq!(
            PendingOrder::from_submit_request(&request, target_exposure.clone()),
            PendingOrder {
                order_id: None,
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure: target_exposure.clone(),
                status: OrderStatus::Submitting,
            }
        );

        let receipt = OrderReceipt {
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            status: OrderStatus::New,
        };
        assert_eq!(
            PendingOrder::from_submit_receipt(&request, target_exposure.clone(), &receipt),
            PendingOrder {
                order_id: Some("order-1".into()),
                client_order_id: "client-1".into(),
                side: Side::Buy,
                price: 94.5,
                quantity: 0.25,
                target_exposure,
                status: OrderStatus::New,
            }
        );

        let observation = OrderObservation {
            order_id: "live-1".into(),
            client_order_id: "live-client-1".into(),
            side: Side::Sell,
            price: 105.5,
            quantity: 0.5,
            realized_pnl: 0.0,
            status: OrderStatus::PartiallyFilled,
        };
        assert_eq!(
            PendingOrder::from_order_observation(&observation, Exposure(-2.0)),
            PendingOrder {
                order_id: Some("live-1".into()),
                client_order_id: "live-client-1".into(),
                side: Side::Sell,
                price: 105.5,
                quantity: 0.5,
                target_exposure: Exposure(-2.0),
                status: OrderStatus::PartiallyFilled,
            }
        );
    }

    #[test]
    fn pending_order_recovery_anchor_only_matches_submitting_without_order_id() {
        let request = OrderRequest {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            side: Side::Buy,
            price: 94.5,
            quantity: 0.25,
            client_order_id: "client-1".into(),
        };
        let anchor = PendingOrder::from_submit_request(&request, Exposure(6.0));
        assert!(anchor.is_submit_recovery_anchor());

        let receipt = OrderReceipt {
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            status: OrderStatus::New,
        };
        assert!(!PendingOrder::from_submit_receipt(&request, Exposure(6.0), &receipt)
            .is_submit_recovery_anchor());

        let observation = OrderObservation {
            order_id: "live-1".into(),
            client_order_id: "live-client-1".into(),
            side: Side::Sell,
            price: 105.5,
            quantity: 0.5,
            realized_pnl: 0.0,
            status: OrderStatus::PartiallyFilled,
        };
        assert!(!PendingOrder::from_order_observation(&observation, Exposure(-2.0))
            .is_submit_recovery_anchor());
    }

    #[test]
    fn pending_order_target_reached_uses_directional_comparison() {
        assert!(PendingOrder::target_reached(Exposure(6.0), Exposure(6.0)));
        assert!(PendingOrder::target_reached(Exposure(6.5), Exposure(6.0)));
        assert!(!PendingOrder::target_reached(Exposure(5.5), Exposure(6.0)));
        assert!(PendingOrder::target_reached(Exposure(-3.0), Exposure(-2.0)));
        assert!(!PendingOrder::target_reached(Exposure(-1.5), Exposure(-2.0)));
        assert!(PendingOrder::target_reached(Exposure(0.0), Exposure(0.0)));
        assert!(!PendingOrder::target_reached(Exposure(2.0), Exposure(0.0)));
        assert!(!PendingOrder::target_reached(Exposure(-2.0), Exposure(0.0)));
    }

    #[test]
    fn submit_recovery_anchor_tracks_submitting_and_receipt_backed_pending_orders() {
        let pending_order = PendingOrder {
            order_id: None,
            client_order_id: "client-1".into(),
            side: Side::Buy,
            price: 94.5,
            quantity: 0.25,
            target_exposure: Exposure(6.0),
            status: OrderStatus::Submitting,
        };
        let anchor = SubmitRecoveryAnchor::from_pending_order(&pending_order).unwrap();
        assert_eq!(anchor.kind, SubmitRecoveryKind::Submitting);
        assert!(anchor.matches(&pending_order));

        let other = PendingOrder {
            client_order_id: "client-2".into(),
            ..pending_order.clone()
        };
        assert!(!anchor.matches(&other));

        let restored = PendingOrder {
            order_id: Some("order-1".into()),
            status: OrderStatus::New,
            ..pending_order
        };
        let restored_anchor = SubmitRecoveryAnchor::from_pending_order(&restored).unwrap();
        assert_eq!(restored_anchor.kind, SubmitRecoveryKind::ReceiptBacked);
        assert!(restored_anchor.matches(&restored));
    }
}
