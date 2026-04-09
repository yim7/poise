use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use poise_core::events::DomainEvent;
use poise_core::risk::{CapacityBudget, validate_capacity_budget};
use poise_core::strategy::TrackConfig;
use poise_core::types::ExchangeRules;
use poise_core::types::Exposure;

use crate::command::TrackCommand;
use crate::executor;
use crate::ledger::{LedgerDelta, LedgerGapRecord};
use crate::observation::{
    MarketObservation, OrderObservation, PositionObservation, TrackObservation,
};
use crate::ports::{ClockPort, ExchangeOrder, OrderReceipt, OrderRequest};
use crate::reconciler;
use crate::runtime::{ExecutorState, TrackRuntime, TrackStatus};
use crate::snapshot::TrackRuntimeSnapshot;
use crate::track::{Instrument, TrackId};
use crate::transition::{TrackEffect, TrackTransition};

const DEFAULT_TICK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeSyncMode {
    RecoverOnly,
    RecoverAndReconcile,
}

impl ExchangeSyncMode {
    pub fn allows_follow_up_reconcile(self) -> bool {
        matches!(self, Self::RecoverAndReconcile)
    }
}

pub struct TrackManager {
    tracks: HashMap<TrackId, TrackRuntime>,
    instruments: HashMap<Instrument, TrackId>,
    clock: Arc<dyn ClockPort>,
}

impl TrackManager {
    pub fn new(clock: Arc<dyn ClockPort>) -> Self {
        Self {
            tracks: HashMap::new(),
            instruments: HashMap::new(),
            clock,
        }
    }

    pub fn add_track(
        &mut self,
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
    ) -> Result<()> {
        self.add_track_with_tick_timeout_secs(
            id,
            instrument,
            config,
            budget,
            exchange_rules,
            DEFAULT_TICK_TIMEOUT_SECS,
        )
    }

    pub fn add_track_with_tick_timeout_secs(
        &mut self,
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        budget: CapacityBudget,
        exchange_rules: ExchangeRules,
        tick_timeout_secs: u64,
    ) -> Result<()> {
        if self.tracks.contains_key(&id) {
            bail!("duplicate track id `{}`", id.as_str());
        }
        if self.instruments.contains_key(&instrument) {
            bail!(
                "duplicate instrument `{}:{}`",
                instrument.venue.as_str(),
                instrument.symbol
            );
        }

        poise_core::strategy::validate_config(&config).map_err(|e| anyhow::anyhow!(e))?;
        validate_capacity_budget(&budget).map_err(|e| anyhow::anyhow!(e))?;
        let track = TrackRuntime::new(
            id.clone(),
            instrument.clone(),
            config,
            budget,
            exchange_rules,
            self.clock.now(),
        );
        let mut track = track;
        track.tick_timeout_secs = tick_timeout_secs;
        self.tracks.insert(id.clone(), track);
        self.instruments.insert(instrument, id);
        Ok(())
    }

    pub fn resolve_track_id(&self, instrument: &Instrument) -> Option<TrackId> {
        self.instruments.get(instrument).cloned()
    }

    pub fn observe(
        &mut self,
        id: &TrackId,
        observation: TrackObservation,
    ) -> Result<TrackTransition> {
        if let TrackObservation::Order(observation) = observation {
            return self
                .observe_order_update(id, observation)
                .map(|(transition, _)| transition);
        }

        let (events, effects) = match observation {
            TrackObservation::Market(observation) => self.observe_market(id, observation)?,
            TrackObservation::Position(observation) => {
                self.observe_position(id, observation)?;
                match self.cached_reference_price(id)? {
                    Some(reference_price) => self.reconcile_track(id, reference_price)?,
                    None => (vec![], vec![]),
                }
            }
            TrackObservation::Order(_) => unreachable!("order observation handled above"),
        };

        self.transition_for(id, events, effects)
    }

    pub fn observe_order_update(
        &mut self,
        id: &TrackId,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, executor::OrderUpdateAbsorbResult)> {
        let should_reconcile = observation.status.should_reconcile_after_order_update();
        let absorb_result = self.observe_order(id, observation)?;
        let (events, effects) = match (should_reconcile, self.cached_reference_price(id)?) {
            (true, Some(reference_price)) => self.reconcile_track(id, reference_price)?,
            _ => (vec![], vec![]),
        };

        Ok((self.transition_for(id, events, effects)?, absorb_result))
    }

    pub fn apply_ledger_adjustment(
        &mut self,
        id: &TrackId,
        deltas: &[LedgerDelta],
        gaps: &[LedgerGapRecord],
    ) -> Result<()> {
        let today = self.clock.now().date_naive();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        if track.ledger_state.is_empty()
            && (track.risk_state.realized_pnl_day.is_some()
                || track.risk_state.realized_pnl_today.abs() > f64::EPSILON
                || track.risk_state.realized_pnl_cumulative.abs() > f64::EPSILON)
        {
            track.ledger_state = crate::ledger::TrackLedgerState::from_legacy_realized(
                track.risk_state.realized_pnl_day,
                track.risk_state.realized_pnl_today,
                track.risk_state.realized_pnl_cumulative,
            );
        }

        for delta in deltas {
            track.ledger_state.apply_delta(today, delta);
        }
        for gap in gaps {
            if track
                .ledger_state
                .unresolved_gaps
                .iter()
                .all(|existing| existing.gap_key != gap.gap_key)
            {
                track.ledger_state.record_gap(gap.clone());
            }
        }
        track.risk_state.realized_pnl_day = track.ledger_state.realized_pnl_day;
        track.risk_state.realized_pnl_today = track.ledger_state.gross_realized_pnl_today;
        track.risk_state.realized_pnl_cumulative = track.ledger_state.gross_realized_pnl_cumulative;
        Ok(())
    }

    pub fn sync_exchange_state(
        &mut self,
        id: &TrackId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        pending_submit_hints: Vec<executor::PendingSubmitHint>,
    ) -> Result<TrackTransition> {
        let (events, effects) = self.apply_exchange_state_sync(
            id,
            position,
            open_orders,
            pending_submit_hints,
            ExchangeSyncMode::RecoverAndReconcile,
        )?;
        self.transition_for(id, events, effects)
    }

    pub fn sync_exchange_state_without_reconcile(
        &mut self,
        id: &TrackId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        pending_submit_hints: Vec<executor::PendingSubmitHint>,
    ) -> Result<TrackTransition> {
        let (events, effects) = self.apply_exchange_state_sync(
            id,
            position,
            open_orders,
            pending_submit_hints,
            ExchangeSyncMode::RecoverOnly,
        )?;
        self.transition_for(id, events, effects)
    }

    pub fn command(&mut self, id: &TrackId, command: TrackCommand) -> Result<TrackTransition> {
        let (events, effects) = match command {
            TrackCommand::Pause => {
                self.pause_track(id.as_str())?;
                (vec![], vec![])
            }
            TrackCommand::Resume => self.resume_track(id.as_str())?,
            TrackCommand::Reconcile => {
                let Some(reference_price) = self
                    .tracks
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
                    .reference_price
                else {
                    return self.transition_for(id, vec![], vec![]);
                };
                self.reconcile_track(id, reference_price)?
            }
            TrackCommand::Terminate => self.terminate_track(id)?,
            TrackCommand::Flatten => self.flatten_track(id)?,
        };

        self.transition_for(id, events, effects)
    }

    pub fn refresh_market_data_health(&mut self, id: &TrackId) -> Result<TrackTransition> {
        let _ = self.guard_market_data_freshness(id)?;
        self.transition_for(id, vec![], vec![])
    }

    pub fn pause_track(&mut self, id: &str) -> Result<()> {
        let track = self
            .tracks
            .get_mut(&TrackId::from(id))
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        if matches!(track.status, TrackStatus::Terminated) {
            bail!("cannot pause terminated track `{id}`");
        }
        // Pause disables strategy targeting, but does not rewrite observed exchange state.
        track.status = TrackStatus::Paused;
        track.desired_exposure = None;
        Ok(())
    }

    pub fn resume_track(&mut self, id: &str) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let track_id = TrackId::from(id);
        let track = self
            .tracks
            .get(&track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        if matches!(track.status, TrackStatus::Terminated) {
            bail!("cannot resume terminated track `{id}`");
        }

        if track.manual_target_override.is_some() {
            let reference_price = {
                let track = self
                    .tracks
                    .get_mut(&track_id)
                    .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
                track.manual_target_override = None;
                track.status = TrackStatus::WaitingMarketData;
                track.desired_exposure = None;
                track.replacement_gate_reason = None;
                track.reference_price
            };

            return match reference_price {
                Some(reference_price) => self.reconcile_track(&track_id, reference_price),
                None => Ok((vec![], vec![])),
            };
        }

        let resumed_at = self.clock.now();
        let resumed_state = {
            let track = self
                .tracks
                .get(&track_id)
                .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;

            if !matches!(track.status, TrackStatus::Paused) {
                bail!("cannot resume track `{id}` from status {:?}", track.status);
            }

            if let Some(reference_price) = track.reference_price {
                let mut resumed = track.clone();
                resumed.status = TrackStatus::WaitingMarketData;
                resumed.executor_state = track.executor_state.reset_for_activation(resumed_at);
                let result = self.plan_inventory_execution_for_track(&resumed, reference_price)?;
                (
                    result.new_status.unwrap_or(TrackStatus::Active),
                    Some(result.desired_exposure.clone()),
                    result.replacement_gate_reason,
                    executor::refresh_state(
                        &resumed.executor_state,
                        &resumed.current_exposure,
                        &result.desired_exposure,
                        resumed.config.min_rebalance_units,
                        resumed_at,
                    ),
                )
            } else {
                (
                    TrackStatus::WaitingMarketData,
                    None,
                    None,
                    track.executor_state.reset_for_activation(resumed_at),
                )
            }
        };

        let track = self
            .tracks
            .get_mut(&track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        let (status, exposure, replacement_gate_reason, executor_state) = resumed_state;
        track.status = status;
        track.desired_exposure = exposure;
        track.replacement_gate_reason = replacement_gate_reason;
        track.executor_state = executor_state;

        Ok((vec![], vec![]))
    }

    fn terminate_track(&mut self, id: &TrackId) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let reference_price = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

            if matches!(track.status, TrackStatus::Terminated) {
                bail!("track `{}` is already terminated", id.as_str());
            }

            track.manual_target_override = None;
            track.status = TrackStatus::Terminated;
            track.desired_exposure = Some(Exposure(0.0));
            track.replacement_gate_reason = None;
            track.reference_price
        };

        match reference_price {
            Some(reference_price) => self.reconcile_track(id, reference_price),
            None => Ok((vec![], vec![])),
        }
    }

    fn flatten_track(&mut self, id: &TrackId) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let reference_price = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

            if matches!(track.status, TrackStatus::Terminated) {
                bail!("cannot flatten terminated track `{}`", id.as_str());
            }

            track.manual_target_override = Some(Exposure(0.0));
            track.status = TrackStatus::ReducingOnly;
            track.reference_price
        };

        match reference_price {
            Some(reference_price) => self.reconcile_track(id, reference_price),
            None => Ok((vec![], vec![])),
        }
    }

    pub fn snapshot(&self, id: &str) -> Option<TrackRuntimeSnapshot> {
        self.get_track(id).map(TrackRuntime::snapshot)
    }

    pub fn restore_track_state(&mut self, snapshot: &TrackRuntimeSnapshot) -> Result<()> {
        let track = self
            .tracks
            .get_mut(&snapshot.track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", snapshot.track_id.as_str()))?;
        if track.instrument != snapshot.instrument {
            bail!(
                "snapshot instrument mismatch for `{}`: expected `{}:{}`, got `{}:{}`",
                snapshot.track_id.as_str(),
                track.instrument.venue.as_str(),
                track.instrument.symbol,
                snapshot.instrument.venue.as_str(),
                snapshot.instrument.symbol
            );
        }
        track.restore_from_snapshot(snapshot)?;
        Ok(())
    }

    pub fn record_submit_request(
        &mut self,
        id: &TrackId,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state =
            executor::record_submit_request(&track.executor_state, request, desired_exposure);
        if next_state != track.executor_state {
            track.executor_state = next_state;
            track.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_submit_receipt(
        &mut self,
        id: &TrackId,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let resolution = executor::record_submit_receipt(
            &track.executor_state,
            request,
            desired_exposure,
            receipt,
        );
        match resolution {
            executor::SubmitReceiptResolution::Recorded { state } => {
                if state != track.executor_state {
                    track.executor_state = state;
                    track.replacement_gate_reason = None;
                }
                Ok(())
            }
            executor::SubmitReceiptResolution::Unmatched => bail!(
                "submit receipt did not match executor slot: track=`{}`, client_order_id=`{}`, order_id=`{}`",
                id.as_str(),
                request.client_order_id,
                receipt.order_id,
            ),
        }
    }

    pub fn record_submit_failure(&mut self, id: &TrackId, client_order_id: &str) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::record_submit_failure(&track.executor_state, client_order_id);
        if next_state != track.executor_state {
            track.executor_state = next_state;
            track.replacement_gate_reason = None;
        }
        Ok(())
    }

    fn clear_working_order_by_order_id(&mut self, id: &TrackId, order_id: &str) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::clear_working_order_by_order_id(&track.executor_state, order_id);
        if next_state != track.executor_state {
            track.executor_state = next_state;
            track.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_cancel_order_success(&mut self, id: &TrackId, order_id: &str) -> Result<()> {
        self.clear_working_order_by_order_id(id, order_id)
    }

    fn clear_all_working_orders(&mut self, id: &TrackId) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::clear_all_working_orders(&track.executor_state);
        if next_state != track.executor_state {
            track.executor_state = next_state;
            track.replacement_gate_reason = None;
        }
        Ok(())
    }

    pub fn record_cancel_all_success(&mut self, id: &TrackId) -> Result<()> {
        self.clear_all_working_orders(id)
    }

    pub fn recover_submit_effect(
        &mut self,
        id: &TrackId,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<executor::SubmitRecoveryPlan> {
        let live_order_observation = live_order.map(|order| OrderObservation {
            order_id: order.order_id.clone(),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.qty,
            realized_pnl: 0.0,
            status: order.status,
        });
        let observed_at = self.clock.now();

        let plan = {
            let track = self
                .tracks
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            let current_plan = self.submit_recovery_plan_context(track, observed_at);
            executor::recover_submit_effect(executor::SubmitRecoveryInput {
                exchange_rules: &track.exchange_rules,
                previous_state: &track.executor_state,
                request,
                desired_exposure: &desired_exposure,
                current_exposure: &track.current_exposure,
                live_order: live_order_observation.as_ref(),
                current_plan,
            })
        };

        {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            if let Some(state) = plan.resolution.state()
                && state != &track.executor_state
            {
                track.executor_state = state.clone();
                track.replacement_gate_reason = None;
            }
        };

        Ok(plan)
    }

    pub fn list_tracks(&self) -> Vec<&TrackRuntime> {
        self.tracks.values().collect()
    }

    pub fn get_track(&self, id: &str) -> Option<&TrackRuntime> {
        self.tracks.get(&TrackId::from(id))
    }

    pub fn clock(&self) -> &dyn ClockPort {
        self.clock.as_ref()
    }

    fn transition_for(
        &self,
        id: &TrackId,
        events: Vec<DomainEvent>,
        effects: Vec<TrackEffect>,
    ) -> Result<TrackTransition> {
        let snapshot = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .snapshot();
        Ok(TrackTransition {
            snapshot,
            events,
            effects,
        })
    }

    fn cached_reference_price(&self, id: &TrackId) -> Result<Option<f64>> {
        Ok(self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .reference_price)
    }

    fn observe_market(
        &mut self,
        id: &TrackId,
        observation: MarketObservation,
    ) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let now = self.clock.now();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.last_tick_at = Some(now);
        track.market_data_stale_since = None;
        self.reconcile_track(id, observation.reference_price)
    }

    fn observe_position(&mut self, id: &TrackId, observation: PositionObservation) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let unit_qty = track.config.base_qty_per_unit();
        track.current_exposure = if unit_qty <= f64::EPSILON {
            poise_core::types::Exposure(0.0)
        } else {
            poise_core::types::Exposure(observation.qty / unit_qty)
        };
        track.risk_state.unrealized_pnl = observation.unrealized_pnl;
        Ok(())
    }

    fn observe_order(
        &mut self,
        id: &TrackId,
        observation: OrderObservation,
    ) -> Result<executor::OrderUpdateAbsorbResult> {
        let today = self.clock.now().date_naive();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        if track.ledger_state.is_empty()
            && (track.risk_state.realized_pnl_day.is_some()
                || track.risk_state.realized_pnl_today.abs() > f64::EPSILON
                || track.risk_state.realized_pnl_cumulative.abs() > f64::EPSILON)
        {
            track.ledger_state = crate::ledger::TrackLedgerState::from_legacy_realized(
                track.risk_state.realized_pnl_day,
                track.risk_state.realized_pnl_today,
                track.risk_state.realized_pnl_cumulative,
            );
        }
        track
            .ledger_state
            .apply_gross_realized_pnl(today, observation.realized_pnl);
        track.risk_state.realized_pnl_day = track.ledger_state.realized_pnl_day;
        track.risk_state.realized_pnl_today = track.ledger_state.gross_realized_pnl_today;
        track.risk_state.realized_pnl_cumulative = track.ledger_state.gross_realized_pnl_cumulative;

        if track.executor_state.diagnostics.recovery_anomaly.is_some() {
            return Ok(executor::OrderUpdateAbsorbResult::DuplicateReplay);
        }

        let applied =
            executor::apply_order_observation_with_result(&track.executor_state, &observation);
        if applied.state != track.executor_state {
            track.executor_state = applied.state;
            track.replacement_gate_reason = None;
        }

        Ok(applied.absorb_result)
    }

    fn apply_exchange_state_sync(
        &mut self,
        id: &TrackId,
        position: PositionObservation,
        open_orders: Vec<OrderObservation>,
        pending_submit_hints: Vec<executor::PendingSubmitHint>,
        mode: ExchangeSyncMode,
    ) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        self.observe_position(id, position)?;
        let observed_at = self.clock.now();
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .clone();
        let previous_state = track.executor_state.clone();
        let recovery = executor::recover_working_orders(executor::RecoveryInput {
            current_exposure: &track.current_exposure,
            desired_exposure: track.desired_exposure.as_ref(),
            min_rebalance_units: track.config.min_rebalance_units,
            previous_state: Some(&previous_state),
            live_orders: &open_orders,
            pending_submit_hints: &pending_submit_hints,
            observed_at,
        });

        match recovery {
            executor::RecoveryResolution::Anomaly { state, .. } => {
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                track.executor_state = state;
                track.replacement_gate_reason = None;
                Ok((vec![], vec![TrackEffect::NoOp]))
            }
            executor::RecoveryResolution::Rebuilt { state } => {
                let mut planned_track = track.clone();
                planned_track.executor_state = state;

                if matches!(planned_track.status, TrackStatus::Paused) {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    track.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                }

                let Some(reference_price) = planned_track.reference_price else {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    track.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                };

                if !mode.allows_follow_up_reconcile() {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    track.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                }

                if self.guard_market_data_freshness(id)? {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    track.replacement_gate_reason = None;
                    return Ok((vec![], vec![]));
                }

                let planned =
                    self.plan_inventory_execution_for_track(&planned_track, reference_price)?;
                let effects = planned
                    .effects
                    .iter()
                    .filter(|effect| match effect {
                        TrackEffect::SubmitOrder { request, .. } => {
                            !pending_submit_hints.iter().any(|hint| {
                                executor::submit_requests_match(
                                    &hint.request,
                                    request,
                                    &planned_track.exchange_rules,
                                )
                            })
                        }
                        _ => true,
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                if let Some(new_status) = planned.new_status {
                    track.status = new_status;
                }
                track.desired_exposure = Some(planned.desired_exposure);
                track.reference_price = Some(reference_price);
                track.replacement_gate_reason = planned.replacement_gate_reason;
                track.executor_state = planned.executor_state;
                Ok((planned.events, effects))
            }
        }
    }

    fn reconcile_track(
        &mut self,
        id: &TrackId,
        reference_price: f64,
    ) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        if self.guard_market_data_freshness(id)? {
            return Ok((vec![], vec![]));
        }

        if matches!(self.tracks[id].status, TrackStatus::Paused) {
            let track = self.tracks.get_mut(id).unwrap();
            track.reference_price = Some(reference_price);
            track.desired_exposure = None;
            track.replacement_gate_reason = None;
            return Ok((vec![], vec![]));
        }

        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let PlannedInventoryExecution {
            mut events,
            effects: planned_effects,
            desired_exposure,
            new_status,
            replacement_gate_reason,
            executor_state,
        } = self.plan_inventory_execution_for_track(track, reference_price)?;
        let effects = planned_effects;

        let track = self.tracks.get_mut(id).unwrap();
        let replacement_gate_event = (track.replacement_gate_reason != replacement_gate_reason)
            .then(|| replacement_gate_reason.clone())
            .flatten()
            .map(|reason| DomainEvent::ReplacementGateApplied { reason });
        if let Some(new_status) = new_status {
            track.status = new_status;
        }
        track.desired_exposure = Some(desired_exposure);
        track.reference_price = Some(reference_price);
        track.replacement_gate_reason = replacement_gate_reason;
        track.executor_state = executor_state;

        if let Some(event) = replacement_gate_event {
            events.push(event);
        }

        Ok((events, effects))
    }

    fn guard_market_data_freshness(&mut self, id: &TrackId) -> Result<bool> {
        let now = self.clock.now();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        let Some(last_tick_at) = track.last_tick_at else {
            return Ok(false);
        };

        let age_ms = (now - last_tick_at).num_milliseconds().max(0);
        if age_ms
            <= i64::try_from(track.tick_timeout_secs)
                .unwrap_or(i64::try_from(DEFAULT_TICK_TIMEOUT_SECS).unwrap_or(30))
                * 1000
        {
            return Ok(false);
        }

        if track.market_data_stale_since.is_none() {
            track.market_data_stale_since = Some(now);
        }

        Ok(true)
    }

    fn submit_recovery_plan_context<'a>(
        &self,
        track: &'a TrackRuntime,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<executor::SubmitIntentInput<'a>> {
        let reference_price = track.reference_price?;
        if matches!(track.status, TrackStatus::Paused) {
            return None;
        }

        let target = reconciler::reconcile_target(track, reference_price);
        (!target.suppress_execution).then_some(self.submit_intent_input(
            track,
            target.desired_exposure,
            reference_price,
            observed_at,
        ))
    }

    fn submit_intent_input<'a>(
        &self,
        track: &'a TrackRuntime,
        desired_exposure: poise_core::types::Exposure,
        reference_price: f64,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> executor::SubmitIntentInput<'a> {
        executor::SubmitIntentInput {
            track_id: &track.id,
            instrument: &track.instrument,
            exchange_rules: &track.exchange_rules,
            base_qty_per_unit: track.config.base_qty_per_unit(),
            min_rebalance_units: track.config.min_rebalance_units,
            current_exposure: track.current_exposure.clone(),
            desired_exposure,
            reference_price,
            observed_at,
        }
    }

    fn plan_inventory_execution_for_track(
        &self,
        track: &TrackRuntime,
        reference_price: f64,
    ) -> Result<PlannedInventoryExecution> {
        let target = reconciler::reconcile_target(track, reference_price);
        if track.executor_state.diagnostics.recovery_anomaly.is_some() {
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![TrackEffect::NoOp],
                desired_exposure: target.desired_exposure,
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state: track.executor_state.clone(),
            });
        }
        let observed_at = self.clock.now();
        if target.suppress_execution {
            let executor_state = executor::refresh_state(
                &track.executor_state,
                &track.current_exposure,
                &target.desired_exposure,
                track.config.min_rebalance_units,
                observed_at,
            );
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![TrackEffect::NoOp],
                desired_exposure: target.desired_exposure.clone(),
                new_status: target.new_status,
                replacement_gate_reason: None,
                executor_state,
            });
        }
        let executor_state = Some(&track.executor_state);
        let submit_intent = self.submit_intent_input(
            track,
            target.desired_exposure.clone(),
            reference_price,
            observed_at,
        );
        let plan = executor::plan(executor::ExecutorInput::new(submit_intent, executor_state));

        Ok(PlannedInventoryExecution {
            events: target.events,
            effects: plan.effects,
            desired_exposure: target.desired_exposure,
            new_status: target.new_status,
            replacement_gate_reason: plan.replacement_gate_reason,
            executor_state: plan.state,
        })
    }
}

struct PlannedInventoryExecution {
    events: Vec<DomainEvent>,
    effects: Vec<TrackEffect>,
    desired_exposure: Exposure,
    new_status: Option<TrackStatus>,
    replacement_gate_reason: Option<poise_core::events::ReplacementGateReason>,
    executor_state: ExecutorState,
}

#[cfg(test)]
fn rounded_values_match(left: f64, right: f64, step: f64) -> bool {
    let tolerance = if step <= f64::EPSILON {
        f64::EPSILON * 16.0
    } else {
        (step * 1e-9).max(f64::EPSILON * 16.0)
    };
    (left - right).abs() <= tolerance
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::executor::{ExecutionMode, ExecutionReason, OrderRole, OrderSlot};
    use crate::ports::*;
    use crate::runtime::{
        ExecutionSlot, ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus,
        WorkingOrder,
    };
    use chrono::{TimeZone, Utc};
    use poise_core::events::ReplacementGateReason;
    use poise_core::strategy::*;
    use poise_core::types::Side;

    struct FakeClock;

    impl ClockPort for FakeClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            Utc::now()
        }
    }

    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    #[derive(Clone)]
    struct MutableClock(Arc<Mutex<chrono::DateTime<Utc>>>);

    impl ClockPort for MutableClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    impl MutableClock {
        fn set(&self, value: chrono::DateTime<Utc>) {
            *self.0.lock().unwrap() = value;
        }
    }

    #[test]
    fn exchange_sync_mode_explicitly_controls_follow_up_reconcile() {
        assert!(!ExchangeSyncMode::RecoverOnly.allows_follow_up_reconcile());
        assert!(ExchangeSyncMode::RecoverAndReconcile.allows_follow_up_reconcile());
    }

    fn test_manager() -> TrackManager {
        TrackManager::new(Arc::new(FakeClock))
    }

    fn test_manager_with_clock(clock: Arc<dyn ClockPort>) -> TrackManager {
        TrackManager::new(clock)
    }

    fn test_config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        }
    }

    fn test_budget() -> CapacityBudget {
        CapacityBudget {
            max_notional: 3000.0,
            daily_loss_limit: -120.0,
            stop_loss_pct: 4.0,
        }
    }

    fn budget_with_max_notional(max_notional: f64) -> CapacityBudget {
        CapacityBudget {
            max_notional,
            ..test_budget()
        }
    }

    fn test_exchange_rules() -> poise_core::types::ExchangeRules {
        poise_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.1,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn test_instrument(symbol: &str) -> Instrument {
        Instrument::new(crate::track::Venue::Binance, symbol)
    }

    fn register_test_track(manager: &mut TrackManager, id: &str, symbol: &str) {
        manager
            .add_track(
                TrackId::new(id),
                test_instrument(symbol),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();
    }

    fn working_order(
        order_id: Option<&str>,
        client_order_id: &str,
        side: poise_core::types::Side,
        price: f64,
        quantity: f64,
        _desired_exposure: poise_core::types::Exposure,
        status: OrderStatus,
    ) -> WorkingOrder {
        WorkingOrder {
            order_id: order_id.map(str::to_string),
            client_order_id: client_order_id.to_string(),
            side,
            price,
            quantity,
            status,
            role: match side {
                poise_core::types::Side::Buy => OrderRole::IncreaseInventory,
                poise_core::types::Side::Sell => OrderRole::DecreaseInventory,
            },
        }
    }

    fn seed_executor_slot(track: &mut TrackRuntime, order: WorkingOrder, state: SlotState) {
        track
            .executor_state
            .slots
            .retain(|slot| slot.slot != OrderSlot::new("inventory_core"));
        if track.executor_state.active_round.is_none() {
            let inferred_target = track.desired_exposure.clone().unwrap_or_else(|| {
                let per_unit = track.config.base_qty_per_unit();
                let delta = if per_unit <= f64::EPSILON {
                    0.0
                } else {
                    order.quantity / per_unit
                };
                let signed_delta = match order.side {
                    poise_core::types::Side::Buy => delta,
                    poise_core::types::Side::Sell => -delta,
                };
                poise_core::types::Exposure(track.current_exposure.0 + signed_delta)
            });
            track.executor_state.active_round = Some(crate::runtime::ExecutionRound {
                desired_exposure: inferred_target,
                mode: track.executor_state.diagnostics.mode.clone(),
                started_at: track.executor_state.stats.started_at,
            });
        }
        track.executor_state.slots.insert(
            0,
            ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state,
                working_order: Some(order),
            },
        );
    }

    fn seed_named_executor_slot(
        track: &mut TrackRuntime,
        slot_name: &str,
        order: WorkingOrder,
        state: SlotState,
    ) {
        track
            .executor_state
            .slots
            .retain(|slot| slot.slot != OrderSlot::new(slot_name));
        if track.executor_state.active_round.is_none() {
            let inferred_target = track.desired_exposure.clone().unwrap_or_else(|| {
                let per_unit = track.config.base_qty_per_unit();
                let delta = if per_unit <= f64::EPSILON {
                    0.0
                } else {
                    order.quantity / per_unit
                };
                let signed_delta = match order.side {
                    poise_core::types::Side::Buy => delta,
                    poise_core::types::Side::Sell => -delta,
                };
                poise_core::types::Exposure(track.current_exposure.0 + signed_delta)
            });
            track.executor_state.active_round = Some(crate::runtime::ExecutionRound {
                desired_exposure: inferred_target,
                mode: track.executor_state.diagnostics.mode.clone(),
                started_at: track.executor_state.stats.started_at,
            });
        }
        track.executor_state.slots.push(ExecutionSlot {
            slot: OrderSlot::new(slot_name),
            state,
            working_order: Some(order),
        });
    }

    fn working_order_from_submit_request(
        request: &OrderRequest,
        _desired_exposure: poise_core::types::Exposure,
    ) -> WorkingOrder {
        WorkingOrder {
            order_id: None,
            client_order_id: request.client_order_id.clone(),
            side: request.side,
            price: request.price,
            quantity: request.quantity,
            status: OrderStatus::Submitting,
            role: if request.reduce_only {
                OrderRole::DecreaseInventory
            } else {
                OrderRole::IncreaseInventory
            },
        }
    }

    fn inventory_core_order(track: &TrackRuntime) -> Option<&WorkingOrder> {
        track
            .executor_state
            .slots
            .iter()
            .find(|slot| slot.slot == OrderSlot::new("inventory_core"))
            .and_then(|slot| slot.working_order.as_ref())
    }

    fn inventory_core_order_from_snapshot(
        snapshot: &TrackRuntimeSnapshot,
    ) -> Option<&WorkingOrder> {
        snapshot
            .executor_state
            .slots
            .iter()
            .find(|slot| slot.slot == OrderSlot::new("inventory_core"))
            .and_then(|slot| slot.working_order.as_ref())
    }

    fn empty_inventory_core_slot() -> ExecutionSlot {
        ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state: SlotState::Empty,
            working_order: None,
        }
    }

    fn active_runtime_with_executor_order() -> TrackRuntime {
        let mut track = TrackRuntime::new(
            TrackId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(4.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));
        seed_executor_slot(
            &mut track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        track.risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 12.5,
            realized_pnl_cumulative: 17.5,
            unrealized_pnl: -3.0,
            ..RiskState::default()
        };
        track.reference_price = Some(95.0);
        track.out_of_band_since = Some(Utc.with_ymd_and_hms(2026, 3, 24, 7, 30, 0).unwrap());
        track
    }

    fn passive_executor_state_with_matching_buy_order() -> ExecutorState {
        ExecutorState {
            active_round: Some(crate::runtime::ExecutionRound {
                desired_exposure: poise_core::types::Exposure(4.0),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            }),
            diagnostics: crate::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: poise_core::types::Exposure(4.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: None,
            },
            slots: vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::Working,
                working_order: Some(WorkingOrder {
                    order_id: Some("order-1".into()),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 15.0,
                    status: OrderStatus::New,
                    role: OrderRole::IncreaseInventory,
                }),
            }],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: poise_core::types::Exposure(4.0),
                max_gap_age_ms: 0,
            },
        }
    }

    fn test_manager_with_active_track() -> TrackManager {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc-core", "BTCUSDT");
        manager
    }

    fn test_manager_with_cached_price(reference_price: f64) -> TrackManager {
        let mut manager = test_manager_with_active_track();
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.reference_price = Some(reference_price);
        manager
    }

    #[test]
    fn add_track_validates_config() {
        let mut manager = test_manager();
        let bad_config = TrackConfig {
            lower_price: 110.0,
            upper_price: 90.0,
            ..test_config()
        };
        assert!(
            manager
                .add_track(
                    TrackId::new("test"),
                    test_instrument("BTCUSDT"),
                    bad_config,
                    test_budget(),
                    test_exchange_rules(),
                )
                .is_err()
        );
    }

    #[test]
    fn add_track_rejects_non_positive_max_notional() {
        let mut manager = test_manager();
        let error = manager
            .add_track(
                TrackId::new("test"),
                test_instrument("BTCUSDT"),
                test_config(),
                CapacityBudget {
                    max_notional: 0.0,
                    ..test_budget()
                },
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("max_notional"));
    }

    #[test]
    fn add_track_rejects_non_negative_daily_loss_limit() {
        let mut manager = test_manager();
        let error = manager
            .add_track(
                TrackId::new("test"),
                test_instrument("BTCUSDT"),
                test_config(),
                CapacityBudget {
                    daily_loss_limit: 0.0,
                    ..test_budget()
                },
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("daily_loss_limit"));
    }

    #[test]
    fn add_track_rejects_non_positive_stop_loss_pct() {
        let mut manager = test_manager();
        let error = manager
            .add_track(
                TrackId::new("test"),
                test_instrument("BTCUSDT"),
                test_config(),
                CapacityBudget {
                    stop_loss_pct: 0.0,
                    ..test_budget()
                },
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("stop_loss_pct"));
    }

    #[test]
    fn add_and_list_tracks() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        register_test_track(&mut manager, "eth1", "ETHUSDT");

        assert_eq!(manager.list_tracks().len(), 2);
        assert!(manager.get_track("btc1").is_some());
        assert!(manager.get_track("eth1").is_some());
        assert!(manager.get_track("nonexistent").is_none());
    }

    #[test]
    fn add_track_stores_budget_on_runtime() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.budget, test_budget());
    }

    #[test]
    fn add_track_initializes_executor_state_from_activation_clock() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(started_at)));

        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.get_track("btc1").unwrap();
        let executor_state = &track.executor_state;
        assert_eq!(executor_state.slots, vec![empty_inventory_core_slot()]);
        assert_eq!(executor_state.diagnostics.inventory_gap, Exposure(0.0));
        assert_eq!(executor_state.diagnostics.gap_started_at, None);
        assert_eq!(executor_state.stats.started_at, started_at);
    }

    #[test]
    fn add_track_rejects_duplicate_track_ids() {
        let mut manager = test_manager();
        let track_id = TrackId::new("btc-core");
        manager
            .add_track(
                track_id.clone(),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager
            .add_track(
                track_id,
                test_instrument("ETHUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("duplicate track id"));
    }

    #[test]
    fn add_track_rejects_duplicate_instruments() {
        let mut manager = test_manager();
        manager
            .add_track(
                TrackId::new("btc-core"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap();

        let error = manager
            .add_track(
                TrackId::new("btc-alt"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("duplicate instrument"));
    }

    #[test]
    fn resolve_track_id_returns_registered_track_id_for_instrument() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc-core", "BTCUSDT");

        assert_eq!(
            manager.resolve_track_id(&test_instrument("BTCUSDT")),
            Some(TrackId::new("btc-core"))
        );
    }

    #[test]
    fn snapshot_roundtrip_preserves_runtime_state() {
        let runtime = active_runtime_with_executor_order();
        let snapshot = runtime.snapshot();
        let mut restored = TrackRuntime::new(
            TrackId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            test_budget(),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        restored.restore_from_snapshot(&snapshot).unwrap();

        assert_eq!(restored.snapshot(), snapshot);
    }

    #[test]
    fn restore_track_state_rejects_config_mismatch() {
        let mut manager = test_manager_with_active_track();
        let snapshot = {
            let mut runtime = TrackRuntime::new(
                TrackId::new("btc-core"),
                test_instrument("BTCUSDT"),
                TrackConfig {
                    lower_price: 80.0,
                    ..test_config()
                },
                test_budget(),
                test_exchange_rules(),
                Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
            );
            runtime.status = TrackStatus::Active;
            runtime.current_exposure = poise_core::types::Exposure(0.0);
            runtime.reference_price = Some(90.0);
            runtime.snapshot()
        };

        let error = manager.restore_track_state(&snapshot).unwrap_err();
        assert!(error.to_string().contains("snapshot config mismatch"));
    }

    #[test]
    fn restore_from_snapshot_keeps_runtime_budget_during_reconcile() {
        let mut runtime = TrackRuntime::new(
            TrackId::new("btc-core"),
            test_instrument("BTCUSDT"),
            test_config(),
            budget_with_max_notional(1500.0),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = poise_core::types::Exposure(0.0);
        runtime.reference_price = Some(90.0);

        let snapshot = {
            let mut source = TrackRuntime::new(
                TrackId::new("btc-core"),
                test_instrument("BTCUSDT"),
                test_config(),
                test_budget(),
                test_exchange_rules(),
                Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
            );
            source.status = TrackStatus::Active;
            source.current_exposure = poise_core::types::Exposure(0.0);
            source.reference_price = Some(90.0);
            source.snapshot()
        };

        runtime.restore_from_snapshot(&snapshot).unwrap();

        let result = crate::reconciler::reconcile_target(&runtime, 90.0);
        assert_eq!(runtime.budget.max_notional, 1500.0);
        assert_eq!(result.desired_exposure, poise_core::types::Exposure(4.0));
    }

    #[test]
    fn observe_market_reconciles_and_returns_effects() {
        let mut manager = test_manager_with_active_track();
        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                crate::observation::TrackObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert!(!transition.effects.is_empty());
        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert!(!transition.events.is_empty());
    }

    #[test]
    fn observe_market_plans_through_inventory_executor() {
        let mut manager = test_manager_with_active_track();
        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                crate::observation::TrackObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));
        assert!(!transition.snapshot.executor_state.slots.is_empty());
        assert!(
            !transition
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::CancelAll { .. }))
        );
    }

    #[test]
    fn executor_noop_when_working_orders_match_desired_orders() {
        let mut manager = test_manager_with_active_track();
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.executor_state = passive_executor_state_with_matching_buy_order();

        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                crate::observation::TrackObservation::Market(
                    crate::observation::MarketObservation {
                        reference_price: 95.0,
                    },
                ),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        let executor_state = transition.snapshot.executor_state;
        assert_eq!(executor_state.slots.len(), 1);
        assert_eq!(
            executor_state.slots,
            passive_executor_state_with_matching_buy_order().slots
        );
    }

    #[test]
    fn command_reconcile_uses_cached_reference_price() {
        let mut manager = test_manager_with_cached_price(95.0);
        {
            let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
            track.budget = CapacityBudget {
                max_notional: 1500.0,
                ..test_budget()
            };
        }
        let transition = manager
            .command(
                &TrackId::new("btc-core"),
                crate::command::TrackCommand::Reconcile,
            )
            .unwrap();

        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert_eq!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
        assert!(!transition.effects.is_empty());
    }

    #[test]
    fn observe_market_updates_track() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();
        assert!(!transition.events.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.status, TrackStatus::Active);
        assert_eq!(track.reference_price, Some(95.0));
        assert_eq!(track.current_exposure.0, 0.0);
        assert!(track.desired_exposure.as_ref().unwrap().0 > 0.0); // should be long below center
    }

    #[test]
    fn observe_market_returns_transition_with_effects_and_events() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(!transition.effects.is_empty());
        assert!(!transition.events.is_empty());
    }

    #[test]
    fn observe_market_updates_target_without_faking_current_exposure() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.current_exposure = poise_core::types::Exposure(2.0);

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(!transition.events.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.current_exposure.0, 2.0);
        assert_eq!(track.desired_exposure.as_ref().unwrap().0, 4.0);
        assert_eq!(track.reference_price, Some(95.0));
    }

    #[test]
    fn observe_market_updates_desired_exposure_without_changing_protocol_target_projection() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.desired_exposure,
            Some(poise_core::types::Exposure(4.0))
        );
        assert_eq!(
            transition.snapshot.desired_exposure,
            Some(poise_core::types::Exposure(4.0))
        );
    }

    #[test]
    fn resolve_track_id_returns_none_for_unknown_instrument() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        assert_eq!(manager.resolve_track_id(&test_instrument("ETHUSDT")), None);
    }

    #[test]
    fn paused_track_ignores_reconcile_updates() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(transition.events.is_empty());
        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.status, TrackStatus::Paused);
        assert_eq!(track.current_exposure.0, 2.0);
        assert_eq!(track.desired_exposure, None);
        assert_eq!(track.reference_price, Some(95.0));
    }

    #[test]
    fn observe_market_keeps_submit_pending_slot_without_extra_effects() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));
        seed_executor_slot(
            track,
            working_order(
                None,
                "recover-1",
                poise_core::types::Side::Buy,
                94.0,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.desired_exposure,
            Some(poise_core::types::Exposure(4.0))
        );
        assert_eq!(transition.snapshot.observed.reference_price, Some(95.0));
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order(
                None,
                "recover-1",
                poise_core::types::Side::Buy,
                94.0,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ))
        );
        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
    }

    #[test]
    fn observe_market_keeps_strategy_target_while_suppressing_small_rebalance() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.notional_per_unit = 100.0;
        track.config.min_rebalance_units = 0.5;

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 97.0,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        assert!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .is_some_and(|target| (target.0 - 2.4).abs() < 0.001)
        );
        assert!(
            transition
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
    }

    #[test]
    fn observe_market_keeps_latest_strategy_target_while_preserving_active_execution_anchor() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.notional_per_unit = 100.0;
        track.config.min_rebalance_units = 0.5;
        let expected_target = poise_core::strategy::desired_exposure(96.125, &track.config);
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                96.0,
                0.8,
                poise_core::types::Exposure(2.8),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 96.125,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        assert!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .is_some_and(|target| (target.0 - expected_target.0).abs() < 0.001)
        );
        assert!(
            transition
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ExposureTargetChanged { .. }))
        );
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                96.0,
                0.8,
                poise_core::types::Exposure(2.8),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn observe_market_keeps_submit_pending_slot_when_small_rebalance_is_below_min_rebalance_units()
    {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.notional_per_unit = 100.0;
        track.config.min_rebalance_units = 0.5;
        seed_executor_slot(
            track,
            working_order(
                None,
                "recover-1",
                poise_core::types::Side::Buy,
                95.0,
                1.5,
                poise_core::types::Exposure(3.5),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 97.0,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        assert!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .is_some_and(|target| (target.0 - 2.4).abs() < 0.001)
        );
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order(
                None,
                "recover-1",
                poise_core::types::Side::Buy,
                95.0,
                1.5,
                poise_core::types::Exposure(3.5),
                OrderStatus::Submitting,
            ))
        );
    }

    #[test]
    fn observe_market_records_submit_pending_slot_for_new_submit_effect() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(0.0);

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        let (request, desired_exposure) = match transition.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                },
            ] => (request, desired_exposure),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order_from_submit_request(
                request,
                desired_exposure.clone(),
            ))
        );
    }

    #[test]
    fn observe_market_replacement_gate_emits_event_when_reason_first_appears() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.min_rebalance_units = 0.0;
        track.exchange_rules = poise_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Sell,
                99.9,
                7.0,
                poise_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
        assert!(transition.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason: ReplacementGateReason::RoundedMatch,
            }
        )));
    }

    #[test]
    fn observe_market_replacement_gate_deduplicates_same_reason_across_ticks() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.min_rebalance_units = 0.0;
        track.exchange_rules = poise_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Sell,
                99.9,
                7.0,
                poise_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let first = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();
        let second = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert!(first.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason: ReplacementGateReason::RoundedMatch,
            }
        )));
        assert!(
            !second
                .events
                .iter()
                .any(|event| matches!(event, DomainEvent::ReplacementGateApplied { .. }))
        );
        assert_eq!(
            second.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::RoundedMatch)
        );
    }

    #[test]
    fn observe_market_replacement_gate_emits_event_when_reason_changes() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.config.min_rebalance_units = 0.0;
        track.exchange_rules.maker_fee_rate = 0.0002;
        track.exchange_rules.taker_fee_rate = 0.0004;
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                100.0,
                0.1,
                poise_core::types::Exposure(0.4),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        track.replacement_gate_reason = Some(ReplacementGateReason::RoundedMatch);

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 99.95,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.replacement_gate_reason,
            Some(ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps: 10.0,
                threshold_bps: 21.0,
            })
        );
        assert!(transition.events.iter().any(|event| matches!(
            event,
            DomainEvent::ReplacementGateApplied {
                reason:
                    ReplacementGateReason::ImprovementBelowThreshold {
                        improvement_bps,
                        threshold_bps,
                    },
            } if (*improvement_bps - 10.0).abs() < f64::EPSILON
                && (*threshold_bps - 21.0).abs() < f64::EPSILON
        )));
    }

    #[test]
    fn resume_track_rejects_non_paused_status() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let error = manager.resume_track("btc1").unwrap_err();

        assert!(error.to_string().contains("cannot resume"));
    }

    #[test]
    fn terminated_track_keeps_zero_target_even_when_price_is_in_band() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track_id = TrackId::new("btc1");

        let track = manager.tracks.get_mut(&track_id).unwrap();
        track.status = TrackStatus::Terminated;
        track.current_exposure = Exposure(0.0);

        let transition = manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert_eq!(transition.snapshot.status, TrackStatus::Terminated);
        assert_eq!(transition.snapshot.desired_exposure, Some(Exposure(0.0)));
        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
    }

    #[test]
    fn flatten_persists_manual_target_override_and_targets_zero() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track_id = TrackId::new("btc-core");

        let transition = manager.command(&track_id, TrackCommand::Flatten).unwrap();

        let track = manager.get_track("btc-core").unwrap();
        assert_eq!(track.manual_target_override, Some(Exposure(0.0)));
        assert_eq!(track.status, TrackStatus::ReducingOnly);
        assert_eq!(
            transition.snapshot.manual_target_override,
            Some(Exposure(0.0))
        );
        assert_eq!(transition.snapshot.desired_exposure, Some(Exposure(0.0)));
    }

    #[test]
    fn flatten_keeps_zero_target_even_when_price_is_in_band() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track_id = TrackId::new("btc-core");

        manager.command(&track_id, TrackCommand::Flatten).unwrap();
        let transition = manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 100.0,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.manual_target_override,
            Some(Exposure(0.0))
        );
        assert_eq!(transition.snapshot.desired_exposure, Some(Exposure(0.0)));
        assert_eq!(transition.snapshot.status, TrackStatus::ReducingOnly);
    }

    #[test]
    fn resume_clears_manual_target_override_after_flatten() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track_id = TrackId::new("btc-core");

        manager.command(&track_id, TrackCommand::Flatten).unwrap();
        manager.resume_track("btc-core").unwrap();

        let track = manager.get_track("btc-core").unwrap();
        assert!(track.manual_target_override.is_none());
        assert_ne!(track.status, TrackStatus::ReducingOnly);
    }

    #[test]
    fn resume_track_recomputes_status_from_last_price() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = poise_core::types::Exposure(8.0);
        track.reference_price = Some(85.0);
        track.budget = CapacityBudget {
            max_notional: 1500.0,
            ..test_budget()
        };

        manager.resume_track("btc1").unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.status, TrackStatus::Frozen);
        assert_eq!(track.current_exposure.0, 8.0);
        assert_eq!(
            track.desired_exposure.as_ref().map(|target| target.0),
            Some(4.0)
        );
    }

    #[test]
    fn resume_track_preserves_active_execution_anchor_when_last_price_drift_stays_within_threshold()
    {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.reference_price = Some(99.95);
        track.exchange_rules = poise_core::types::ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.5,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Sell,
                99.9,
                7.0,
                poise_core::types::Exposure(0.5),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        manager.resume_track("btc1").unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.status, TrackStatus::Active);
        assert_eq!(track.replacement_gate_reason, None);
    }

    #[test]
    fn resume_track_resets_execution_stats_for_new_activation() {
        let resumed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(resumed_at)));
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = Exposure(2.0);
        track.reference_price = Some(95.0);
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                Side::Buy,
                95.0,
                2.0,
                Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        let executor_state = &mut track.executor_state;
        executor_state.diagnostics.inventory_gap = Exposure(2.0);
        executor_state.diagnostics.gap_started_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap());
        executor_state.stats.started_at = Utc.with_ymd_and_hms(2026, 3, 29, 7, 30, 0).unwrap();
        executor_state.stats.max_inventory_gap_abs = Exposure(6.0);
        executor_state.stats.max_gap_age_ms = 120_000;

        manager.resume_track("btc1").unwrap();

        let track = manager.get_track("btc1").unwrap();
        let executor_state = &track.executor_state;
        assert_eq!(executor_state.slots.len(), 1);
        assert_eq!(executor_state.stats.started_at, resumed_at);
        assert_eq!(executor_state.stats.max_inventory_gap_abs, Exposure(2.0));
        assert_eq!(executor_state.stats.max_gap_age_ms, 0);
        assert_eq!(executor_state.diagnostics.gap_started_at, Some(resumed_at));
    }

    #[test]
    fn resume_track_does_not_stage_submit_pending_without_emitting_effects() {
        let resumed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(resumed_at)));
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = Exposure(0.0);
        track.reference_price = Some(95.0);

        manager.resume_track("btc1").unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.executor_state.slots,
            vec![empty_inventory_core_slot()]
        );
        assert_eq!(track.executor_state.diagnostics.last_reprice_at, None);

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        assert!(matches!(
            transition.effects.as_slice(),
            [TrackEffect::SubmitOrder { .. }]
        ));
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .slots
                .first()
                .map(|slot| slot.state.clone()),
            Some(SlotState::SubmitPending)
        );
    }

    #[test]
    fn record_submit_receipt_updates_inventory_core_slot() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: poise_core::types::Side::Buy,
            price: 95.0,
            quantity: 0.4,
            reduce_only: false,
        };
        let receipt = OrderReceipt {
            order_id: "order-1".into(),
            client_order_id: "client-1".into(),
            status: OrderStatus::New,
        };

        manager
            .record_submit_request(
                &TrackId::new("btc1"),
                &request,
                poise_core::types::Exposure(4.0),
            )
            .unwrap();
        manager
            .record_submit_receipt(
                &TrackId::new("btc1"),
                &request,
                poise_core::types::Exposure(4.0),
                &receipt,
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            inventory_core_order(track),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                95.0,
                0.4,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn record_submit_receipt_rejects_receipt_without_matching_executor_slot() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let error = manager
            .record_submit_receipt(
                &TrackId::new("btc1"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                poise_core::types::Exposure(4.0),
                &OrderReceipt {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    status: OrderStatus::New,
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("submit receipt"));
    }

    #[test]
    fn record_submit_receipt_accepts_matching_receipt_even_when_state_is_unchanged() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        seed_executor_slot(
            manager.tracks.get_mut(&TrackId::new("btc1")).unwrap(),
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                95.0,
                0.4,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        manager
            .record_submit_receipt(
                &TrackId::new("btc1"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 95.0,
                    quantity: 0.4,
                    reduce_only: false,
                },
                poise_core::types::Exposure(4.0),
                &OrderReceipt {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    status: OrderStatus::New,
                },
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            inventory_core_order(track),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                95.0,
                0.4,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn record_submit_failure_clears_submit_pending_slot_by_client_order_id() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: poise_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            reduce_only: false,
        };
        seed_executor_slot(
            manager.tracks.get_mut(&TrackId::new("btc1")).unwrap(),
            working_order_from_submit_request(&request, poise_core::types::Exposure(4.0)),
            SlotState::SubmitPending,
        );

        manager
            .record_submit_failure(&TrackId::new("btc1"), &request.client_order_id)
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert!(inventory_core_order(track).is_none());
    }

    #[test]
    fn recover_submit_effect_supersedes_without_receipt_evidence_when_target_is_reached() {
        let mut manager = test_manager_with_cached_price(92.5);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.current_exposure = poise_core::types::Exposure(6.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 92.5,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    reduce_only: false,
                },
                poise_core::types::Exposure(6.0),
                None,
            )
            .unwrap();

        assert!(matches!(
            recovery.resolution,
            executor::SubmitRecoveryResolution::Superseded { .. }
        ));
        assert!(recovery.effects.is_empty());
        assert!(inventory_core_order(manager.get_track("btc-core").unwrap()).is_none());
    }

    #[test]
    fn recover_submit_effect_supersede_plan_is_executor_owned() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));
        seed_executor_slot(
            track,
            working_order(
                None,
                "btc-core-reconcile",
                poise_core::types::Side::Buy,
                94.0,
                test_config().base_qty_per_unit() * 6.0,
                poise_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.0,
                    quantity: test_config().base_qty_per_unit() * 6.0,
                    reduce_only: false,
                },
                poise_core::types::Exposure(6.0),
                None,
            )
            .unwrap();

        let executor::SubmitRecoveryResolution::Superseded { state } = &recovery.resolution else {
            panic!("expected stale submit effect to be superseded");
        };
        assert!(matches!(
            recovery.effects.as_slice(),
            [TrackEffect::SubmitOrder {
                request,
                desired_exposure,
            }] if request.side == poise_core::types::Side::Buy
                && rounded_values_match(request.price, 95.0, test_exchange_rules().price_tick)
                && rounded_values_match(
                    request.quantity,
                    test_config().base_qty_per_unit() * 4.0,
                    test_exchange_rules().quantity_step,
                )
                && *desired_exposure == poise_core::types::Exposure(4.0)
        ));
        let replacement_pending = match recovery.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                },
            ] => Some(working_order_from_submit_request(
                request,
                desired_exposure.clone(),
            )),
            _ => None,
        };
        assert_eq!(
            state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: replacement_pending.clone(),
            }]
        );
        assert_eq!(
            inventory_core_order(manager.get_track("btc-core").unwrap()),
            replacement_pending.as_ref()
        );
    }

    #[test]
    fn recover_submit_effect_supersedes_when_reduce_only_semantics_change() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.desired_exposure = Some(poise_core::types::Exposure(4.0));
        track.executor_state.active_round = Some(crate::runtime::ExecutionRound {
            desired_exposure: poise_core::types::Exposure(-2.0),
            mode: ExecutionMode::Passive,
            started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
        });
        track.executor_state.slots = vec![ExecutionSlot {
            slot: OrderSlot::new("inventory_core"),
            state: SlotState::SubmitPending,
            working_order: Some(WorkingOrder {
                order_id: None,
                client_order_id: "btc-core-reconcile".into(),
                side: poise_core::types::Side::Buy,
                price: 95.0,
                quantity: test_config().base_qty_per_unit() * 4.0,
                status: OrderStatus::Submitting,
                role: OrderRole::DecreaseInventory,
            }),
        }];

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 95.0,
                    quantity: test_config().base_qty_per_unit() * 4.0,
                    reduce_only: true,
                },
                poise_core::types::Exposure(-2.0),
                None,
            )
            .unwrap();

        let executor::SubmitRecoveryResolution::Superseded { state } = &recovery.resolution else {
            panic!("expected reduce_only mismatch to supersede stale submit effect");
        };
        assert!(matches!(
            recovery.effects.as_slice(),
            [TrackEffect::SubmitOrder {
                request,
                desired_exposure,
            }] if request.side == poise_core::types::Side::Buy
                && rounded_values_match(request.price, 95.0, test_exchange_rules().price_tick)
                && rounded_values_match(
                    request.quantity,
                    test_config().base_qty_per_unit() * 4.0,
                    test_exchange_rules().quantity_step,
                )
                && !request.reduce_only
                && *desired_exposure == poise_core::types::Exposure(4.0)
        ));
        let replacement_pending = match recovery.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure: _,
                },
            ] => Some(WorkingOrder {
                order_id: None,
                client_order_id: request.client_order_id.clone(),
                side: request.side,
                price: request.price,
                quantity: request.quantity,
                status: OrderStatus::Submitting,
                role: OrderRole::IncreaseInventory,
            }),
            _ => None,
        };
        assert_eq!(
            state.slots,
            vec![ExecutionSlot {
                slot: OrderSlot::new("inventory_core"),
                state: SlotState::SubmitPending,
                working_order: replacement_pending.clone(),
            }]
        );
        assert_eq!(
            inventory_core_order(manager.get_track("btc-core").unwrap()),
            replacement_pending.as_ref()
        );
    }

    #[test]
    fn recover_submit_effect_proceeds_when_current_plan_keeps_same_rounded_order_request_within_anchor_threshold()
     {
        let mut manager = test_manager_with_cached_price(94.99);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.current_exposure = poise_core::types::Exposure(0.0);
        track.config.notional_per_unit = 100.0;
        track.exchange_rules = poise_core::types::ExchangeRules {
            price_tick: 10.0,
            quantity_step: 1.0,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        };
        track.desired_exposure = Some(poise_core::types::Exposure(4.0));
        seed_executor_slot(
            track,
            working_order(
                None,
                "btc-core-reconcile",
                poise_core::types::Side::Buy,
                90.0,
                4.0,
                poise_core::types::Exposure(4.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 90.0,
                    quantity: 4.0,
                    reduce_only: false,
                },
                poise_core::types::Exposure(4.0),
                None,
            )
            .unwrap();
        let executor::SubmitRecoveryResolution::Proceed {
            desired_exposure, ..
        } = recovery.resolution
        else {
            panic!("matching rounded request should keep the pending submit proceeding");
        };
        assert!(recovery.effects.is_empty());
        assert_eq!(desired_exposure, poise_core::types::Exposure(4.0));
        assert!(matches!(
            inventory_core_order(manager.get_track("btc-core").unwrap()),
            Some(WorkingOrder {
                order_id: None,
                client_order_id,
                side: poise_core::types::Side::Buy,
                price,
                quantity,
                status: OrderStatus::Submitting,
                role: _,
            }) if client_order_id == "btc-core-reconcile"
                && (*price - 90.0).abs() < f64::EPSILON
                && (*quantity - 4.0).abs() < f64::EPSILON
        ));
        assert_eq!(
            manager
                .get_track("btc-core")
                .unwrap()
                .executor_state
                .active_round
                .as_ref()
                .map(|round| round.desired_exposure.clone()),
            Some(poise_core::types::Exposure(4.0))
        );
    }

    #[test]
    fn recover_submit_effect_proceeds_with_pending_submit_when_latest_target_drift_is_within_min_rebalance_units_of_anchor()
     {
        let mut manager = test_manager_with_cached_price(96.125);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.notional_per_unit = 100.0;
        track.config.min_rebalance_units = 0.5;
        seed_executor_slot(
            track,
            working_order(
                None,
                "btc-core-reconcile",
                poise_core::types::Side::Buy,
                96.0,
                0.8,
                poise_core::types::Exposure(2.8),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 96.0,
                    quantity: 0.8,
                    reduce_only: false,
                },
                poise_core::types::Exposure(2.8),
                None,
            )
            .unwrap();
        let executor::SubmitRecoveryResolution::Proceed {
            desired_exposure, ..
        } = recovery.resolution
        else {
            panic!(
                "pending submit should continue when drift stays within the active anchor threshold"
            );
        };
        assert!(recovery.effects.is_empty());
        assert_eq!(desired_exposure, poise_core::types::Exposure(2.8));
        assert!(matches!(
            inventory_core_order(manager.get_track("btc-core").unwrap()),
            Some(WorkingOrder {
                order_id: None,
                client_order_id,
                side: poise_core::types::Side::Buy,
                price,
                quantity,
                status: OrderStatus::Submitting,
                role: _,
            }) if client_order_id == "btc-core-reconcile"
                && (*price - 96.0).abs() < f64::EPSILON
                && (*quantity - 0.8).abs() < f64::EPSILON
        ));
        assert_eq!(
            manager
                .get_track("btc-core")
                .unwrap()
                .executor_state
                .active_round
                .as_ref()
                .map(|round| round.desired_exposure.clone()),
            Some(poise_core::types::Exposure(2.8))
        );
    }

    #[test]
    fn recover_submit_effect_supersedes_pending_submit_when_track_is_paused_and_has_no_current_plan()
     {
        let mut manager = test_manager_with_cached_price(96.125);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.status = TrackStatus::Paused;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.config.notional_per_unit = 100.0;
        track.config.min_rebalance_units = 0.5;
        seed_executor_slot(
            track,
            working_order(
                None,
                "btc-core-reconcile",
                poise_core::types::Side::Buy,
                96.0,
                0.8,
                poise_core::types::Exposure(2.8),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let recovery = manager
            .recover_submit_effect(
                &TrackId::new("btc-core"),
                &OrderRequest {
                    instrument: test_instrument("BTCUSDT"),
                    client_order_id: "btc-core-reconcile".into(),
                    side: poise_core::types::Side::Buy,
                    price: 96.0,
                    quantity: 0.8,
                    reduce_only: false,
                },
                poise_core::types::Exposure(2.8),
                None,
            )
            .unwrap();

        let executor::SubmitRecoveryResolution::Superseded { .. } = recovery.resolution else {
            panic!("paused track should supersede pending submit instead of proceeding");
        };
        assert!(recovery.effects.is_empty());
        assert!(inventory_core_order(manager.get_track("btc-core").unwrap()).is_none());
    }

    #[test]
    fn observe_position_converts_qty_to_exposure_and_updates_unrealized_pnl() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Position(PositionObservation {
                    qty: 15.0,
                    unrealized_pnl: 12.5,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.current_exposure, poise_core::types::Exposure(4.0));
        assert!((track.risk_state.unrealized_pnl - 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_position_with_cached_reference_price_reconciles_immediately() {
        let mut manager = test_manager_with_cached_price(95.0);

        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                TrackObservation::Position(PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 11.0,
                }),
            )
            .unwrap();

        assert_eq!(
            transition.snapshot.current_exposure,
            poise_core::types::Exposure(2.0)
        );
        assert_eq!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
        assert!((transition.snapshot.risk.unrealized_pnl - 11.0).abs() < f64::EPSILON);
        assert!(
            transition
                .effects
                .iter()
                .any(|effect| matches!(effect, TrackEffect::SubmitOrder { .. }))
        );
    }

    #[test]
    fn stale_market_data_suspends_follow_up_reconcile_without_overwriting_status() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap();
        let clock = MutableClock(Arc::new(Mutex::new(started_at)));
        let mut manager = test_manager_with_clock(Arc::new(clock.clone()));
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track_id = TrackId::new("btc1");

        manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        clock.set(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());
        let transition = manager
            .observe(
                &track_id,
                TrackObservation::Position(PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                }),
            )
            .unwrap();

        assert!(transition.effects.is_empty());
        assert!(
            transition
                .snapshot
                .observed
                .market_data_stale_since
                .is_some()
        );
        assert_eq!(transition.snapshot.status, TrackStatus::Active);
    }

    #[test]
    fn fresh_tick_clears_market_data_stale_flag() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap();
        let clock = MutableClock(Arc::new(Mutex::new(started_at)));
        let mut manager = test_manager_with_clock(Arc::new(clock.clone()));
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track_id = TrackId::new("btc1");

        manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        clock.set(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());
        let _ = manager
            .observe(
                &track_id,
                TrackObservation::Position(PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                }),
            )
            .unwrap();

        let transition = manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 96.0,
                }),
            )
            .unwrap();

        assert!(
            transition
                .snapshot
                .observed
                .market_data_stale_since
                .is_none()
        );
    }

    #[test]
    fn sync_exchange_state_clears_stale_inventory_core_slot_when_pending_submit_effect_is_not_preserved()
     {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        seed_executor_slot(
            manager.tracks.get_mut(&TrackId::new("btc1")).unwrap(),
            working_order(
                Some("stale-1"),
                "stale-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 15.0,
                    unrealized_pnl: 12.5,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.current_exposure, poise_core::types::Exposure(4.0));
        assert!(inventory_core_order(track).is_none());
    }

    #[test]
    fn sync_exchange_state_preserves_submit_pending_slot_before_replaying_open_orders() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        seed_executor_slot(
            track,
            working_order(
                None,
                "restore-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::Submitting,
            ),
            SlotState::SubmitPending,
        );

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "restore-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.current_exposure, poise_core::types::Exposure(2.0));
        assert_eq!(
            inventory_core_order(track),
            Some(&working_order(
                Some("live-1"),
                "restore-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn sync_exchange_state_skips_follow_up_reconcile_when_market_data_is_stale() {
        let started_at = Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap();
        let clock = MutableClock(Arc::new(Mutex::new(started_at)));
        let mut manager = test_manager_with_clock(Arc::new(clock.clone()));
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track_id = TrackId::new("btc1");

        manager
            .observe(
                &track_id,
                TrackObservation::Market(MarketObservation {
                    reference_price: 95.0,
                }),
            )
            .unwrap();

        clock.set(Utc.with_ymd_and_hms(2026, 3, 29, 8, 1, 0).unwrap());
        let transition = manager
            .sync_exchange_state(
                &track_id,
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 0.0,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.effects.is_empty());
        assert!(
            transition
                .snapshot
                .observed
                .market_data_stale_since
                .is_some()
        );
    }

    #[test]
    fn sync_exchange_state_keeps_paused_track_target_none() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Paused;
        track.desired_exposure = None;
        track.reference_price = Some(95.0);

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 0.0,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(track.status, TrackStatus::Paused);
        assert_eq!(track.desired_exposure, None);
        assert_eq!(track.reference_price, Some(95.0));
    }

    #[test]
    fn sync_exchange_state_marks_attention_required_when_receipt_backed_order_is_missing() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = poise_core::types::Exposure(2.0);
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));
        track.reference_price = Some(95.0);
        seed_executor_slot(
            track,
            working_order(
                Some("restore-1"),
                "restore-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![executor::PendingSubmitHint {
                    request: OrderRequest {
                        instrument: test_instrument("BTCUSDT"),
                        client_order_id: "restore-1".into(),
                        side: poise_core::types::Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        reduce_only: false,
                    },
                    desired_exposure: poise_core::types::Exposure(6.0),
                }],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.desired_exposure,
            Some(poise_core::types::Exposure(6.0))
        );
        assert!(inventory_core_order(track).is_none());
        assert_eq!(
            track.executor_state.diagnostics.recovery_anomaly.as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
    }

    #[test]
    fn sync_exchange_state_ignores_pending_submit_effect_without_matching_executor_slot() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.executor_state.slots,
            vec![empty_inventory_core_slot()]
        );
        assert!(inventory_core_order(track).is_none());
    }

    #[test]
    fn sync_exchange_state_replays_live_open_order_without_changing_realized_pnl() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        manager
            .tracks
            .get_mut(&TrackId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
            ..RiskState::default()
        };

        manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![OrderObservation {
                    order_id: "live-1".into(),
                    client_order_id: "live-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }],
                vec![],
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((track.risk_state.realized_pnl_today - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn sync_exchange_state_rebuilds_multiple_live_open_orders_when_they_match_distinct_slots() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        seed_executor_slot(
            track,
            working_order(
                Some("order-a"),
                "client-a",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        seed_named_executor_slot(
            track,
            "inventory_followup",
            working_order(
                Some("order-b"),
                "client-b",
                poise_core::types::Side::Sell,
                95.5,
                0.15,
                poise_core::types::Exposure(2.0),
                OrderStatus::PartiallyFilled,
            ),
            SlotState::Working,
        );

        let transition = manager
            .sync_exchange_state(
                &TrackId::new("btc1"),
                PositionObservation {
                    qty: 7.5,
                    unrealized_pnl: 3.0,
                },
                vec![
                    OrderObservation {
                        order_id: "order-b".into(),
                        client_order_id: "client-b".into(),
                        side: poise_core::types::Side::Sell,
                        price: 95.5,
                        quantity: 0.15,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                    OrderObservation {
                        order_id: "order-a".into(),
                        client_order_id: "client-a".into(),
                        side: poise_core::types::Side::Buy,
                        price: 94.5,
                        quantity: 0.25,
                        realized_pnl: 0.0,
                        status: OrderStatus::New,
                    },
                ],
                vec![],
            )
            .unwrap();

        assert!(transition.events.is_empty());
        assert!(transition.effects.is_empty());
        let track = manager.get_track("btc1").unwrap();
        assert!(track.executor_state.diagnostics.recovery_anomaly.is_none());
        assert_eq!(track.executor_state.slots.len(), 2);
        assert_eq!(
            track.executor_state.slots[0].slot,
            OrderSlot::new("inventory_core")
        );
        assert_eq!(
            track.executor_state.slots[0]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-a")
        );
        assert_eq!(
            track.executor_state.slots[1].slot,
            OrderSlot::new("inventory_followup")
        );
        assert_eq!(
            track.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-b")
        );
    }

    #[test]
    fn observe_order_promotes_matching_pending_slot_for_open_status() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: poise_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            reduce_only: false,
        };
        manager
            .record_submit_request(
                &TrackId::new("btc1"),
                &request,
                poise_core::types::Exposure(6.0),
            )
            .unwrap();

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            inventory_core_order(track),
            Some(&working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(6.0),
                OrderStatus::New,
            ))
        );
    }

    #[test]
    fn apply_execution_ledger_event_updates_order_and_ledger_in_one_step() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let request = OrderRequest {
            instrument: test_instrument("BTCUSDT"),
            client_order_id: "client-1".into(),
            side: poise_core::types::Side::Buy,
            price: 94.5,
            quantity: 0.25,
            reduce_only: false,
        };
        manager
            .record_submit_request(
                &TrackId::new("btc1"),
                &request,
                poise_core::types::Exposure(6.0),
            )
            .unwrap();

        let (_, absorb_result) = manager
            .observe_order_update(
                &TrackId::new("btc1"),
                OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -12.5,
                    status: OrderStatus::PartiallyFilled,
                },
            )
            .unwrap();

        assert_eq!(
            absorb_result,
            crate::executor::OrderUpdateAbsorbResult::Applied
        );
        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            inventory_core_order(track).map(|order| order.status),
            Some(OrderStatus::PartiallyFilled)
        );

        let snapshot = serde_json::to_value(manager.snapshot("btc1").unwrap()).unwrap();
        assert_eq!(
            snapshot["ledger_state"]["gross_realized_pnl_cumulative"],
            json!(-12.5)
        );
    }

    #[test]
    fn observe_order_does_not_mutate_slots_while_recovery_anomaly_is_active() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.desired_exposure = Some(poise_core::types::Exposure(6.0));
        track.executor_state = ExecutorState {
            active_round: Some(crate::runtime::ExecutionRound {
                desired_exposure: poise_core::types::Exposure(6.0),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            }),
            diagnostics: crate::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: poise_core::types::Exposure(6.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            },
            slots: vec![empty_inventory_core_slot()],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: poise_core::types::Exposure(6.0),
                max_gap_age_ms: 0,
            },
        };

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert!(inventory_core_order(track).is_none());
        assert_eq!(
            track.executor_state.diagnostics.recovery_anomaly.as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert_eq!(
            track.executor_state.slots,
            vec![empty_inventory_core_slot()]
        );
    }

    #[test]
    fn canceled_order_keeps_attention_required_while_recovery_anomaly_is_active() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.desired_exposure = Some(poise_core::types::Exposure(4.0));
        track.executor_state = ExecutorState {
            active_round: Some(crate::runtime::ExecutionRound {
                desired_exposure: poise_core::types::Exposure(4.0),
                mode: ExecutionMode::Passive,
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap(),
            }),
            diagnostics: crate::runtime::ExecutorDiagnostics {
                mode: ExecutionMode::Passive,
                inventory_gap: poise_core::types::Exposure(4.0),
                gap_started_at: Some(Utc.with_ymd_and_hms(2026, 3, 29, 8, 0, 0).unwrap()),
                last_reprice_at: None,
                last_execution_reason: Some(ExecutionReason::GapEnteredPassive),
                recovery_anomaly: Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder),
            },
            slots: vec![empty_inventory_core_slot()],
            recent_terminal_orders: Vec::new(),
            stats: ExecutionStats {
                started_at: Utc.with_ymd_and_hms(2026, 3, 29, 7, 55, 0).unwrap(),
                max_inventory_gap_abs: poise_core::types::Exposure(4.0),
                max_gap_age_ms: 0,
            },
        };

        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .diagnostics
                .recovery_anomaly
                .as_ref(),
            Some(&crate::executor::RecoveryAnomaly::UnknownLiveOrder)
        );
        assert!(inventory_core_order_from_snapshot(&transition.snapshot).is_none());
    }

    #[test]
    fn observe_market_updates_gap_stats_when_execution_is_suppressed() {
        let observed_at = Utc.with_ymd_and_hms(2026, 3, 29, 10, 30, 0).unwrap();
        let mut manager = test_manager_with_clock(Arc::new(FixedClock(observed_at)));
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        track.status = TrackStatus::Active;
        track.current_exposure = Exposure(2.0);
        track.desired_exposure = Some(Exposure(4.0));
        track.executor_state.diagnostics.inventory_gap = Exposure(2.0);
        track.executor_state.diagnostics.gap_started_at =
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 10, 0, 0).unwrap());
        track.executor_state.stats.started_at =
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 45, 0).unwrap();

        let transition = manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Market(MarketObservation {
                    reference_price: 85.0,
                }),
            )
            .unwrap();

        assert_eq!(transition.effects, vec![TrackEffect::NoOp]);
        assert_eq!(transition.snapshot.status, TrackStatus::Frozen);
        assert_eq!(
            transition.snapshot.executor_state.diagnostics.inventory_gap,
            Exposure(2.0)
        );
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .diagnostics
                .gap_started_at,
            Some(Utc.with_ymd_and_hms(2026, 3, 29, 10, 0, 0).unwrap())
        );
        assert_eq!(
            transition
                .snapshot
                .executor_state
                .stats
                .max_inventory_gap_abs,
            Exposure(2.0)
        );
        assert_eq!(
            transition.snapshot.executor_state.stats.max_gap_age_ms,
            30 * 60 * 1000
        );
    }

    #[test]
    fn observe_canceled_order_with_cached_reference_price_reconciles_immediately() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.desired_exposure = Some(poise_core::types::Exposure(4.0));
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Canceled,
                }),
            )
            .unwrap();

        let (request, desired_exposure) = match transition.effects.as_slice() {
            [
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                },
            ] => (request, desired_exposure),
            other => panic!("expected one submit effect, got {other:?}"),
        };
        assert_eq!(
            inventory_core_order_from_snapshot(&transition.snapshot),
            Some(&working_order_from_submit_request(
                request,
                desired_exposure.clone(),
            ))
        );
    }

    #[test]
    fn observe_filled_order_does_not_reconcile_before_position_update() {
        let mut manager = test_manager_with_cached_price(95.0);
        let track = manager.tracks.get_mut(&TrackId::new("btc-core")).unwrap();
        track.desired_exposure = Some(poise_core::types::Exposure(4.0));
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );

        let transition = manager
            .observe(
                &TrackId::new("btc-core"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -12.5,
                    status: OrderStatus::Filled,
                }),
            )
            .unwrap();

        assert!(transition.effects.is_empty());
        assert!(inventory_core_order_from_snapshot(&transition.snapshot).is_none());
        assert!((transition.snapshot.risk.realized_pnl_today + 12.5).abs() < f64::EPSILON);
        assert_eq!(
            transition
                .snapshot
                .desired_exposure
                .as_ref()
                .map(|target| target.0),
            Some(4.0)
        );
    }

    #[test]
    fn observe_order_clears_matching_inventory_core_slot_on_terminal_status() {
        let mut manager = test_manager();
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        let track = manager.tracks.get_mut(&TrackId::new("btc1")).unwrap();
        seed_executor_slot(
            track,
            working_order(
                Some("order-1"),
                "client-1",
                poise_core::types::Side::Buy,
                94.5,
                0.25,
                poise_core::types::Exposure(4.0),
                OrderStatus::New,
            ),
            SlotState::Working,
        );
        track.executor_state.slots.push(ExecutionSlot {
            slot: OrderSlot::new("inventory_followup"),
            state: SlotState::Working,
            working_order: Some(working_order(
                Some("order-2"),
                "client-2",
                poise_core::types::Side::Sell,
                95.5,
                0.15,
                poise_core::types::Exposure(2.0),
                OrderStatus::PartiallyFilled,
            )),
        });

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: 0.0,
                    status: OrderStatus::Filled,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert!(inventory_core_order(track).is_none());
        assert_eq!(track.executor_state.slots.len(), 2);
        assert_eq!(
            track.executor_state.slots[0].slot,
            OrderSlot::new("inventory_core")
        );
        assert_eq!(track.executor_state.slots[0].state, SlotState::Empty);
        assert!(track.executor_state.slots[0].working_order.is_none());
        assert_eq!(
            track.executor_state.slots[1].slot,
            OrderSlot::new("inventory_followup")
        );
        assert_eq!(
            track.executor_state.slots[1]
                .working_order
                .as_ref()
                .and_then(|order| order.order_id.as_deref()),
            Some("order-2")
        );
    }

    #[test]
    fn observe_order_accumulates_realized_pnl_by_utc_day() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_track(&mut manager, "btc1", "BTCUSDT");

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -12.5,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((track.risk_state.realized_pnl_today + 12.5).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_order_resets_realized_pnl_when_utc_day_changes() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        manager
            .tracks
            .get_mut(&TrackId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
            ..RiskState::default()
        };

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((track.risk_state.realized_pnl_today + 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn observe_order_keeps_cumulative_realized_pnl_when_utc_day_changes() {
        let clock = Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0).unwrap(),
        ));
        let mut manager = test_manager_with_clock(clock);
        register_test_track(&mut manager, "btc1", "BTCUSDT");
        manager
            .tracks
            .get_mut(&TrackId::new("btc1"))
            .unwrap()
            .risk_state = RiskState {
            realized_pnl_day: Some(
                Utc.with_ymd_and_hms(2026, 3, 24, 8, 0, 0)
                    .unwrap()
                    .date_naive(),
            ),
            realized_pnl_today: 20.0,
            realized_pnl_cumulative: 20.0,
            unrealized_pnl: 0.0,
            ..RiskState::default()
        };

        manager
            .observe(
                &TrackId::new("btc1"),
                TrackObservation::Order(OrderObservation {
                    order_id: "order-1".into(),
                    client_order_id: "client-1".into(),
                    side: poise_core::types::Side::Buy,
                    price: 94.5,
                    quantity: 0.25,
                    realized_pnl: -5.0,
                    status: OrderStatus::PartiallyFilled,
                }),
            )
            .unwrap();

        let track = manager.get_track("btc1").unwrap();
        assert_eq!(
            track.risk_state.realized_pnl_day,
            Some(
                Utc.with_ymd_and_hms(2026, 3, 25, 1, 0, 0)
                    .unwrap()
                    .date_naive()
            )
        );
        assert!((track.risk_state.realized_pnl_today + 5.0).abs() < f64::EPSILON);
        assert!((track.risk_state.realized_pnl_cumulative - 15.0).abs() < f64::EPSILON);
    }
}
