use chrono::{DateTime, Utc};
use poise_core::strategy::TrackConfig;
use sha2::{Digest, Sha256};

use poise_core::types::Exposure;

use crate::execution_gate::ExecutionGateState;
#[cfg(any(test, feature = "test-support"))]
use crate::executor::{BindingStatus, RecoveryAnomaly};
use crate::ledger::TrackPnlStats;
use crate::ports::OrderReceipt;
use crate::risk_exposure_gate::AppliedRiskAcquisitionRelease;
#[cfg(any(test, feature = "test-support"))]
use crate::risk_exposure_gate::RiskExposureGateState;
#[cfg(any(test, feature = "test-support"))]
use crate::runtime::{AutoState, ControlState};
use crate::runtime::{
    ExecutorState, FreshAcquisitionSession, InventoryDecisionSnapshot, RiskState, TrackState,
};
use poise_core::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackMutationFrameRevision(String);

impl TrackMutationFrameRevision {
    pub fn for_track(instrument: &Instrument, track_config: &TrackConfig) -> Self {
        let payload = serde_json::json!({
            "instrument": instrument,
            "track_config": track_config,
        });
        let mut hasher = Sha256::new();
        hasher.update(payload.to_string().as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackMutationFrame {
    pub(crate) track_id: TrackId,
    pub(crate) frame_revision: TrackMutationFrameRevision,
    pub(crate) runtime_state: TrackState,
    pub(crate) current_exposure: Exposure,
    pub(crate) current_position_qty: f64,
    pub(crate) desired_exposure: Option<Exposure>,
    pub(crate) inventory_decision: Option<InventoryDecisionSnapshot>,
    pub(crate) inventory_decision_revision: u64,
    pub(crate) applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
    pub(crate) fresh_acquisition_session: Option<FreshAcquisitionSession>,
    pub(crate) executor_state: ExecutorState,
    pub(crate) execution_gate_state: ExecutionGateState,
    pub(crate) pnl_stats: TrackPnlStats,
    pub(crate) risk: RiskState,
    pub(crate) out_of_band_since: Option<DateTime<Utc>>,
    pub(crate) market_data_stale_since: Option<DateTime<Utc>>,
}

impl TrackMutationFrame {
    #[cfg(any(test, feature = "test-support"))]
    pub fn status(&self) -> crate::runtime::TrackStatus {
        self.runtime_state.status()
    }

    // Public methods intentionally expose commit/rollback facts instead of the
    // underlying runtime structs.
    pub fn runtime_state(&self) -> &TrackState {
        &self.runtime_state
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_runtime_state(&mut self, runtime_state: TrackState) {
        self.runtime_state = runtime_state;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_risk_exposure_gate_state(
        &mut self,
        frontier: Exposure,
        anchor_price: f64,
        anchor_target: Exposure,
        stale_since: DateTime<Utc>,
    ) {
        self.runtime_state =
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate: RiskExposureGateState {
                    risk_release_frontier: frontier,
                    release_anchor_price: anchor_price,
                    release_anchor_target: anchor_target,
                    stale_since,
                },
            }));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn current_exposure(&self) -> &Exposure {
        &self.current_exposure
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn desired_exposure(&self) -> Option<&Exposure> {
        self.desired_exposure.as_ref()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn inventory_decision(&self) -> Option<&InventoryDecisionSnapshot> {
        self.inventory_decision.as_ref()
    }

    pub fn inventory_decision_revision(&self) -> Option<u64> {
        self.inventory_decision
            .as_ref()
            .map(|decision| decision.revision)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fresh_acquisition_pending(&self) -> bool {
        matches!(
            self.fresh_acquisition_session,
            Some(FreshAcquisitionSession::Pending { .. })
        )
    }

    pub fn market_data_stale(&self) -> bool {
        self.market_data_stale_since.is_some()
    }

    pub fn executor_view_changed(&self, next: &Self) -> bool {
        self.executor_state.view() != next.executor_state.view()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_exposure_state(
        &mut self,
        current_exposure: Exposure,
        desired_exposure: Option<Exposure>,
    ) {
        self.current_exposure = current_exposure;
        self.desired_exposure = desired_exposure;
    }

    pub fn pnl_stats(&self) -> &TrackPnlStats {
        &self.pnl_stats
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_pnl_stats(&mut self, pnl_stats: TrackPnlStats) {
        self.pnl_stats = pnl_stats;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_unrealized_pnl(&mut self, unrealized_pnl: f64) {
        self.risk.unrealized_pnl = unrealized_pnl;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn unrealized_pnl(&self) -> f64 {
        self.risk.unrealized_pnl
    }

    pub fn recovery_anomaly_active(&self) -> bool {
        self.executor_state.recovery_anomaly.is_some()
    }

    pub fn has_recorded_order_fact_for_client_order_id(&self, client_order_id: &str) -> bool {
        self.executor_state.bindings.iter().any(|binding| {
            binding.request.client_order_id == client_order_id && binding.order_id.is_some()
        }) || self
            .executor_state
            .recent_terminal_orders
            .iter()
            .any(|order| order.client_order_id == client_order_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_recovery_anomaly(&mut self, recovery_anomaly: Option<RecoveryAnomaly>) {
        self.executor_state.recovery_anomaly = recovery_anomaly;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_executor_bindings(&mut self) {
        self.executor_state.bindings.clear();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_executor_bindings(&self) -> bool {
        !self.executor_state.bindings.is_empty()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn executor_ledger_anchor_exposure(&self) -> &Exposure {
        &self.executor_state.ledger_state.ledger_anchor_exposure
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_market_data_stale_since(&mut self, stale_since: Option<DateTime<Utc>>) {
        self.market_data_stale_since = stale_since;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn binding_receipt_for_client_order_id(
        &self,
        client_order_id: &str,
    ) -> Option<(Option<String>, BindingStatus)> {
        self.executor_state
            .bindings
            .iter()
            .find(|binding| binding.request.client_order_id == client_order_id)
            .map(|binding| (binding.order_id.clone(), binding.status))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_binding_order_status_for_client_order_id(
        &mut self,
        client_order_id: &str,
        order_id: Option<String>,
        status: BindingStatus,
    ) -> bool {
        let Some(binding) = self
            .executor_state
            .bindings
            .iter_mut()
            .find(|binding| binding.request.client_order_id == client_order_id)
        else {
            return false;
        };
        binding.order_id = order_id;
        binding.status = status;
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn binding_is_active_for_client_order_id(&self, client_order_id: &str) -> Option<bool> {
        self.executor_state
            .bindings
            .iter()
            .find(|binding| binding.request.client_order_id == client_order_id)
            .map(|binding| binding.is_active())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn has_active_binding_for_order_id(&self, order_id: &str) -> bool {
        self.executor_state
            .bindings
            .iter()
            .any(|binding| binding.is_active() && binding.order_id.as_deref() == Some(order_id))
    }

    pub fn has_absorbed_binding_for_cancel_receipt(
        &self,
        order_id: &str,
        receipt: &OrderReceipt,
    ) -> bool {
        self.executor_state.bindings.iter().any(|binding| {
            binding.absorbed_exposure_qty > f64::EPSILON
                && (binding.order_id.as_deref() == Some(order_id)
                    || binding.order_id.as_deref() == Some(receipt.order_id.as_str())
                    || binding.request.client_order_id == receipt.client_order_id)
        })
    }
}
