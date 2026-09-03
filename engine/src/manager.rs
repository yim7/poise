use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use poise_core::events::DomainEvent;
use poise_core::instrument::InstrumentSpec;
use poise_core::types::Exposure;

use crate::clock::ClockPort;
use crate::command::TrackCommand;
use crate::exchange::{ExchangeOrder, ExecutionQuote, OrderReceipt, OrderRequest};
use crate::execution_gate::ExecutionGateDecision;
use crate::execution_plan::{DecisionReason, TrackEffect};
use crate::executor;
use crate::ledger::{TrackPnlRecord, TrackPnlStats};
use crate::observation::{
    CompleteOpenOrderSnapshot, MarketObservation, OrderObservation, PositionObservation,
    TrackObservation,
};
use crate::price_gate::{SubmitPurpose, evaluate_price_execution_gate};
use crate::reconciler;
use crate::risk_exposure_gate::{AppliedRiskAcquisitionRelease, FreshAcquisitionInitialization};
use crate::runtime::{
    AutoState, ControlState, ExchangeObservationRevision, ExecutorState,
    FreshSessionExternalInputs, InventoryDecisionSnapshot, ManualState, QuoteHealthView,
    StrategyPriceStatus, StrategyTargetView, TerminationCause, TrackLiveView, TrackRuntime,
    TrackRuntimeView, TrackState,
};
use crate::transition::TrackTransition;
use poise_core::track::{DEFAULT_TICK_TIMEOUT_SECS, Instrument, TrackDefinition, TrackId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeSyncMode {
    RecoverOnly,
    RecoverAndReconcile,
}

#[derive(Debug, Clone)]
pub enum MarketMutationOutcome {
    LiveOnly,
    Durable(Box<TrackTransition>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitRecoveryOrderLookup {
    NotQueried,
    Complete(Option<ExchangeOrder>),
}

impl SubmitRecoveryOrderLookup {
    fn live_order(&self) -> Option<&ExchangeOrder> {
        match self {
            Self::NotQueried => None,
            Self::Complete(live_order) => live_order.as_ref(),
        }
    }

    fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }
}

impl ExchangeSyncMode {
    pub fn allows_follow_up_reconcile(self) -> bool {
        matches!(self, Self::RecoverAndReconcile)
    }
}

#[derive(Clone)]
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
        definition: TrackDefinition,
        instrument_spec: InstrumentSpec,
    ) -> Result<()> {
        self.add_track_with_tick_timeout_secs(
            definition.clone(),
            instrument_spec,
            definition.tick_timeout_secs(),
        )
    }

    pub fn add_track_with_tick_timeout_secs(
        &mut self,
        definition: TrackDefinition,
        instrument_spec: InstrumentSpec,
        tick_timeout_secs: u64,
    ) -> Result<()> {
        let id = definition.track_id().clone();
        let instrument = definition.instrument().clone();
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

        let track = TrackRuntime::with_tick_timeout_secs(
            definition,
            instrument_spec,
            self.clock.now(),
            tick_timeout_secs,
        );
        self.tracks.insert(id.clone(), track);
        self.instruments.insert(instrument, id);
        Ok(())
    }

    pub fn resolve_track_id(&self, instrument: &Instrument) -> Option<TrackId> {
        self.instruments.get(instrument).cloned()
    }

    pub fn exchange_observation_revision(
        &self,
        id: &TrackId,
    ) -> Result<ExchangeObservationRevision> {
        self.tracks
            .get(id)
            .map(TrackRuntime::exchange_observation_revision)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))
    }

    pub fn retain_exchange_observation_revision(
        &mut self,
        id: &TrackId,
        revision: ExchangeObservationRevision,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.set_exchange_observation_revision(revision);
        Ok(())
    }

    pub fn observe(
        &mut self,
        id: &TrackId,
        observation: TrackObservation,
    ) -> Result<TrackTransition> {
        if let TrackObservation::Market(observation) = observation {
            return match self.observe_market_mutation(id, observation)? {
                MarketMutationOutcome::LiveOnly => self.transition_for(id, vec![], vec![]),
                MarketMutationOutcome::Durable(transition) => Ok(*transition),
            };
        }

        if let TrackObservation::Order(observation) = observation {
            return self
                .observe_order_update(id, observation)
                .map(|(transition, _)| transition);
        }

        let (events, effects) = match observation {
            TrackObservation::Position(observation) => {
                self.observe_position(id, observation)?;
                match self.live_strategy_price(id)? {
                    Some(strategy_price) => self.reconcile_track(id, strategy_price)?,
                    None => (vec![], vec![]),
                }
            }
            TrackObservation::Market(_) | TrackObservation::Order(_) => {
                unreachable!("market/order observation handled above")
            }
        };

        self.transition_for(id, events, effects)
    }

    pub fn observe_market_mutation(
        &mut self,
        id: &TrackId,
        observation: MarketObservation,
    ) -> Result<MarketMutationOutcome> {
        let previous_state = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .clone();
        let (events, effects) = self.observe_market(id, observation)?;
        let candidate_state = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .clone();

        if market_mutation_requires_durable_write(
            &previous_state,
            &candidate_state,
            &events,
            &effects,
        ) {
            return Ok(MarketMutationOutcome::Durable(Box::new(
                TrackTransition::new(candidate_state, events, effects),
            )));
        }

        let (
            strategy_price,
            strategy_price_status,
            mark_price,
            best_bid,
            best_ask,
            price_execution_gate,
            last_tick_at,
            desired_exposure,
            inventory_decision,
            inventory_decision_revision,
            applied_risk_acquisition_release,
            fresh_acquisition_session,
            executor_state,
        ) = {
            let track = self
                .tracks
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            (
                track.strategy_price,
                track.strategy_price_status,
                track.mark_price,
                track.best_bid,
                track.best_ask,
                track.price_execution_gate,
                track.last_tick_at,
                track.desired_exposure.clone(),
                track.inventory_decision.clone(),
                track.inventory_decision_revision,
                track.applied_risk_acquisition_release.clone(),
                track.fresh_acquisition_session.clone(),
                track.executor_state.clone(),
            )
        };

        let executor_view_changed = previous_state.executor_view_changed(&candidate_state);
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        *track = previous_state;
        track.desired_exposure = desired_exposure;
        track.inventory_decision = inventory_decision;
        track.inventory_decision_revision = inventory_decision_revision;
        track.applied_risk_acquisition_release = applied_risk_acquisition_release;
        track.fresh_acquisition_session = fresh_acquisition_session;
        retain_live_only_executor_state(track, executor_state, executor_view_changed);
        track.strategy_price = strategy_price;
        track.strategy_price_status = strategy_price_status;
        track.mark_price = mark_price;
        track.best_bid = best_bid;
        track.best_ask = best_ask;
        track.price_execution_gate = price_execution_gate;
        track.last_tick_at = last_tick_at;

        Ok(MarketMutationOutcome::LiveOnly)
    }

    pub fn observe_order_update(
        &mut self,
        id: &TrackId,
        observation: OrderObservation,
    ) -> Result<(TrackTransition, executor::OrderUpdateAbsorbResult)> {
        let should_reconcile = observation.status.should_reconcile_after_order_update();
        let (absorb_result, mut effects) = self.observe_order(id, observation)?;
        let (events, reconcile_effects) = match (should_reconcile, self.live_strategy_price(id)?) {
            (true, Some(strategy_price)) => self.reconcile_track(id, strategy_price)?,
            _ => (vec![], vec![]),
        };
        effects.extend(reconcile_effects);

        Ok((self.transition_for(id, events, effects)?, absorb_result))
    }

    pub fn apply_pnl_record(&mut self, id: &TrackId, record: &TrackPnlRecord) -> Result<()> {
        let pnl_utc_day = self.current_pnl_utc_day();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.pnl_stats.normalize_utc_day(pnl_utc_day);
        track.pnl_stats.apply_record(record);
        Ok(())
    }

    pub fn sync_exchange_state(
        &mut self,
        id: &TrackId,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
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
        open_orders: CompleteOpenOrderSnapshot,
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
                let Some(strategy_price) = self.live_strategy_price(id)? else {
                    return self.transition_for(id, vec![], vec![]);
                };
                self.reconcile_track(id, strategy_price)?
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

    pub fn market_data_health_deadline(
        &self,
        id: &TrackId,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        if track.market_data_stale_since.is_some() {
            return Ok(None);
        }

        let Some(last_tick_at) = track.last_tick_at else {
            return Ok(None);
        };

        let timeout_secs = i64::try_from(track.tick_timeout_secs)
            .unwrap_or(i64::try_from(DEFAULT_TICK_TIMEOUT_SECS).unwrap_or(30));
        Ok(Some(last_tick_at + chrono::Duration::seconds(timeout_secs)))
    }

    pub fn pause_track(&mut self, id: &str) -> Result<()> {
        let track = self
            .tracks
            .get_mut(&TrackId::from(id))
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        if track.track_state.is_terminated() {
            bail!("cannot pause terminated track `{id}`");
        }
        let suspended = match &track.track_state {
            TrackState::Running(control) => control.clone(),
            TrackState::Paused { suspended } => suspended.clone(),
            TrackState::WaitingMarketData | TrackState::Terminated { .. } => {
                ControlState::Automatic(AutoState::FollowingBand)
            }
        };
        // Pause disables strategy targeting, but does not rewrite observed exchange state.
        track.track_state = TrackState::Paused { suspended };
        Self::clear_targeting_state(track);
        Ok(())
    }

    pub fn reset_executor_for_activation(&mut self, id: &TrackId) -> Result<()> {
        let activated_at = self.clock.now();
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let executor_state = track.executor_state.reset_for_activation(
            track.config(),
            track.current_exposure.clone(),
            activated_at,
        );
        track.replace_executor_state(executor_state);
        Ok(())
    }

    pub fn fresh_start_track(
        &mut self,
        id: &TrackId,
        track_state: TrackState,
        pnl_stats: TrackPnlStats,
        external_inputs: FreshSessionExternalInputs,
    ) -> Result<()> {
        let started_at = self.clock.now();
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .clone();
        let fresh = track.fresh_start(track_state, pnl_stats, external_inputs, started_at);
        self.tracks.insert(id.clone(), fresh);
        Ok(())
    }

    pub fn resume_track(&mut self, id: &str) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let track_id = TrackId::from(id);
        let track = self
            .tracks
            .get(&track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        if track.track_state.is_terminated() {
            bail!("cannot resume terminated track `{id}`");
        }

        if matches!(
            track.track_state,
            TrackState::Running(ControlState::Manual(ManualState::Flattened))
        ) {
            let strategy_price = {
                let track = self
                    .tracks
                    .get_mut(&track_id)
                    .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
                track.track_state = TrackState::WaitingMarketData;
                Self::clear_targeting_state(track);
                Self::live_strategy_price_for(track)
            };

            return match strategy_price {
                Some(strategy_price) => self.reconcile_track(&track_id, strategy_price),
                None => Ok((vec![], vec![])),
            };
        }

        let runtime_state = {
            let track = self
                .tracks
                .get(&track_id)
                .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;

            if !matches!(track.track_state, TrackState::Paused { .. }) {
                bail!(
                    "cannot resume track `{id}` from status {:?}",
                    track.track_state.status()
                );
            }
            let TrackState::Paused { suspended } = &track.track_state else {
                unreachable!("paused state checked above")
            };
            let resumed_control = match suspended {
                ControlState::Automatic(_) => ControlState::Automatic(AutoState::FollowingBand),
                ControlState::Manual(manual) => ControlState::Manual(manual.clone()),
            };
            match resumed_control {
                manual @ ControlState::Manual(_) => TrackState::Running(manual),
                automatic @ ControlState::Automatic(_)
                    if Self::live_strategy_price_for(track).is_some() =>
                {
                    TrackState::Running(automatic)
                }
                ControlState::Automatic(_) => TrackState::WaitingMarketData,
            }
        };

        let track = self
            .tracks
            .get_mut(&track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        track.track_state = runtime_state;
        track.execution_gate_state.last_decision = ExecutionGateDecision::Open;
        Self::clear_targeting_state(track);

        Ok((vec![], vec![]))
    }

    fn terminate_track(&mut self, id: &TrackId) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let strategy_price = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

            if track.track_state.is_terminated() {
                bail!("track `{}` is already terminated", id.as_str());
            }

            track.track_state = TrackState::Terminated {
                cause: TerminationCause::ManualCommand,
            };
            Self::apply_targeting_state(track, Some(Exposure(0.0)), None, None);
            Self::live_strategy_price_for(track)
        };

        match strategy_price {
            Some(strategy_price) => self.reconcile_track(id, strategy_price),
            None => Ok((vec![], vec![])),
        }
    }

    fn flatten_track(&mut self, id: &TrackId) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let strategy_price = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

            if track.track_state.is_terminated() {
                bail!("cannot flatten terminated track `{}`", id.as_str());
            }

            track.track_state = TrackState::Running(ControlState::Manual(ManualState::Flattened));
            Self::apply_targeting_state(track, Some(Exposure(0.0)), None, None);
            Self::live_strategy_price_for(track)
        };

        match strategy_price {
            Some(strategy_price) => self.reconcile_track(id, strategy_price),
            None => Ok((vec![], vec![])),
        }
    }

    pub fn track_live_view(&self, id: &TrackId) -> Result<TrackLiveView> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        Ok(self.track_with_current_pnl_day(track).live_view())
    }

    pub fn track_runtime_view(&self, id: &TrackId) -> Result<TrackRuntimeView> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        Ok(self.track_with_current_pnl_day(track).runtime_view())
    }

    pub fn quote_health_view(&self, id: &TrackId) -> Result<QuoteHealthView> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        Ok(track.quote_health_view())
    }

    pub fn strategy_target_view(&self, id: &TrackId) -> Result<StrategyTargetView> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        Ok(self
            .track_with_current_pnl_day(track)
            .strategy_target_view())
    }

    pub fn set_account_capacity_available_notional(
        &mut self,
        id: &str,
        available_notional: Option<f64>,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(&TrackId::new(id))
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        track
            .execution_gate_state
            .account_capacity
            .available_notional = available_notional;
        Ok(())
    }

    pub fn record_submit_receipt(
        &mut self,
        id: &TrackId,
        request: &OrderRequest,
        desired_exposure: poise_core::types::Exposure,
        receipt: &OrderReceipt,
    ) -> Result<TrackTransition> {
        let effects = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            track.advance_exchange_observation_revision();
            let resolution = executor::record_submit_receipt(
                &track.executor_state,
                request,
                desired_exposure,
                receipt,
            );
            match resolution {
                executor::SubmitReceiptResolution::Recorded { state, effects } => {
                    track.replace_executor_state(state);
                    effects
                }
                executor::SubmitReceiptResolution::Unmatched => bail!(
                    "submit receipt did not match executor binding: track=`{}`, client_order_id=`{}`, order_id=`{}`",
                    id.as_str(),
                    request.client_order_id,
                    receipt.order_id,
                ),
            }
        };
        self.transition_for(id, vec![], effects)
    }

    pub fn record_submit_failure(&mut self, id: &TrackId, client_order_id: &str) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::record_submit_failure(&track.executor_state, client_order_id);
        track.replace_executor_state(next_state);
        Ok(())
    }

    pub fn record_submit_failure_by_recovery_token(
        &mut self,
        id: &TrackId,
        recovery_token: &executor::SubmitRecoveryToken,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::record_submit_failure_by_recovery_token(
            &track.executor_state,
            recovery_token,
        );
        track.replace_executor_state(next_state);
        Ok(())
    }

    pub fn record_cancel_order_success(
        &mut self,
        id: &TrackId,
        order_id: &str,
        receipt: &OrderReceipt,
    ) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.advance_exchange_observation_revision();
        let next_state =
            executor::record_cancel_order_receipt(&track.executor_state, order_id, receipt);
        track.replace_executor_state(next_state);
        Ok(())
    }

    fn clear_all_working_orders(&mut self, id: &TrackId) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let next_state = executor::clear_all_working_orders(&track.executor_state);
        track.replace_executor_state(next_state);
        Ok(())
    }

    pub fn record_cancel_all_success(&mut self, id: &TrackId) -> Result<()> {
        self.tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .advance_exchange_observation_revision();
        self.clear_all_working_orders(id)
    }

    pub fn recover_submit_effect(
        &mut self,
        id: &TrackId,
        recovery_token: &executor::SubmitRecoveryToken,
        order_lookup: &SubmitRecoveryOrderLookup,
    ) -> Result<executor::SubmitRecoveryPlan> {
        let live_order = order_lookup.live_order();
        let live_order_observation = live_order.map(|order| OrderObservation {
            order_id: order.order_id.clone(),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.qty,
            filled_qty: order.filled_qty,
            status: order.status,
        });
        let plan = {
            let track = self
                .tracks
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            executor::recover_submit_effect(executor::SubmitRecoveryInput {
                instrument_spec: &track.instrument_spec,
                previous_state: &track.executor_state,
                recovery_token,
                current_exposure: &track.current_exposure,
                live_order: live_order_observation.as_ref(),
            })
        };

        {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            if order_lookup.is_complete() {
                track.advance_exchange_observation_revision();
            }
            if let Some(state) = plan.resolution.state()
                && state != &track.executor_state
            {
                track.replace_executor_state(state.clone());
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

    pub fn install_planned_runtime(&mut self, runtime: TrackRuntime) {
        let id = runtime.id().clone();
        let current = self
            .tracks
            .get(&id)
            .expect("planned runtime must belong to a loaded track");
        assert_eq!(
            current.instrument(),
            runtime.instrument(),
            "planned runtime cannot change track identity"
        );
        self.tracks.insert(id, runtime);
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
        let next_runtime = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .clone();
        Ok(TrackTransition::new(next_runtime, events, effects))
    }

    fn live_strategy_price(&self, id: &TrackId) -> Result<Option<f64>> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        Ok(Self::live_strategy_price_for(track))
    }

    fn live_strategy_price_for(track: &TrackRuntime) -> Option<f64> {
        matches!(track.strategy_price_status, StrategyPriceStatus::Live)
            .then_some(track.strategy_price)
            .flatten()
    }

    fn execution_quote_for_track(track: &TrackRuntime) -> Option<ExecutionQuote> {
        Some(ExecutionQuote {
            best_bid: track.best_bid?,
            best_ask: track.best_ask?,
        })
    }

    fn submit_purpose_for_track(
        &self,
        track: &TrackRuntime,
        desired_exposure: &Exposure,
    ) -> SubmitPurpose {
        if desired_exposure.0.abs() <= f64::EPSILON
            && (track.manual_target_override() == Some(Exposure(0.0))
                || track.track_state.is_terminated())
        {
            return SubmitPurpose::ManualRiskReduction;
        }

        SubmitPurpose::AutoReconcile
    }

    fn observe_market(
        &mut self,
        id: &TrackId,
        observation: MarketObservation,
    ) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        let now = self.clock.now();
        let strategy_price = {
            let track = self
                .tracks
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

            match observation {
                MarketObservation::MarkPrice { mark_price } => {
                    track.mark_price = Some(mark_price);
                }
                MarketObservation::ExecutionQuote { execution_quote } => {
                    track.last_tick_at = Some(now);
                    track.market_data_stale_since = None;
                    track.best_bid = Some(execution_quote.best_bid);
                    track.best_ask = Some(execution_quote.best_ask);
                    let strategy_price =
                        (execution_quote.best_bid + execution_quote.best_ask) / 2.0;
                    track.strategy_price = Some(strategy_price);
                    track.strategy_price_status = StrategyPriceStatus::Live;
                }
            }

            let execution_quote = Self::execution_quote_for_track(track);
            track.price_execution_gate = evaluate_price_execution_gate(
                track.price_execution_gate,
                track.mark_price,
                execution_quote,
            );

            Self::live_strategy_price_for(track)
        };

        match strategy_price {
            Some(strategy_price) => self.reconcile_track(id, strategy_price),
            None => Ok((vec![], vec![])),
        }
    }

    fn observe_position(&mut self, id: &TrackId, observation: PositionObservation) -> Result<()> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.current_exposure = track.exposure_from_position_qty(observation.qty);
        track.current_position_qty = observation.qty;
        track.risk_state.unrealized_pnl = observation.unrealized_pnl;
        track.advance_exchange_observation_revision();
        Ok(())
    }

    fn observe_order(
        &mut self,
        id: &TrackId,
        observation: OrderObservation,
    ) -> Result<(executor::OrderUpdateAbsorbResult, Vec<TrackEffect>)> {
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

        track.advance_exchange_observation_revision();

        if track.executor_state.recovery_anomaly.is_some() {
            return Ok((
                executor::OrderUpdateAbsorbResult::DuplicateReplay,
                Vec::new(),
            ));
        }

        let applied =
            executor::apply_order_observation_with_result(&track.executor_state, &observation);
        let effects = applied.effects;
        track.replace_executor_state(applied.state);

        Ok((applied.absorb_result, effects))
    }

    fn apply_exchange_state_sync(
        &mut self,
        id: &TrackId,
        position: PositionObservation,
        open_orders: CompleteOpenOrderSnapshot,
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
            config: track.config(),
            current_exposure: &track.current_exposure,
            desired_exposure: track.desired_exposure.as_ref(),
            instrument_spec: &track.instrument_spec,
            previous_state: Some(&previous_state),
            open_orders: &open_orders,
            pending_submit_hints: &pending_submit_hints,
            observed_at,
        });

        match recovery {
            executor::RecoveryResolution::Anomaly { state, .. } => {
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                track.replace_executor_state(state);
                Ok((vec![], vec![]))
            }
            executor::RecoveryResolution::Rebuilt {
                state,
                effects: mut recovery_effects,
            } => {
                let mut planned_track = track.clone();
                planned_track.replace_executor_state(state);

                if planned_track.track_state.is_paused() {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.replace_executor_state(planned_track.executor_state);
                    return Ok((vec![], recovery_effects));
                }

                let Some(strategy_price) = Self::live_strategy_price_for(&planned_track) else {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.replace_executor_state(planned_track.executor_state);
                    return Ok((vec![], recovery_effects));
                };

                if !mode.allows_follow_up_reconcile() {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.replace_executor_state(planned_track.executor_state);
                    return Ok((vec![], recovery_effects));
                }

                if self.guard_market_data_freshness(id)? {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.replace_executor_state(planned_track.executor_state);
                    return Ok((vec![], recovery_effects));
                }

                let planned =
                    self.plan_inventory_execution_for_track(&planned_track, strategy_price)?;
                recovery_effects.extend(
                    planned
                        .effects
                        .iter()
                        .filter(|effect| {
                            !matches!(
                                effect,
                                TrackEffect::SubmitOrder { recovery_token, .. }
                                    if pending_submit_hints.iter().any(|hint| {
                                        hint.recovery_token
                                            .matches_submission_identity(recovery_token)
                                    })
                            )
                        })
                        .cloned(),
                );
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                let (events, _) = Self::install_planned_inventory_execution(track, planned);
                Ok((events, recovery_effects))
            }
        }
    }

    fn reconcile_track(
        &mut self,
        id: &TrackId,
        strategy_price: f64,
    ) -> Result<(Vec<DomainEvent>, Vec<TrackEffect>)> {
        if self.guard_market_data_freshness(id)? {
            return Ok((vec![], vec![]));
        }

        if self.tracks[id].track_state.is_paused() {
            let track = self.tracks.get_mut(id).unwrap();
            Self::clear_targeting_state(track);
            return Ok((vec![], vec![]));
        }

        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        let planned = self.plan_inventory_execution_for_track(track, strategy_price)?;

        let track = self.tracks.get_mut(id).unwrap();
        Ok(Self::install_planned_inventory_execution(track, planned))
    }

    fn install_planned_inventory_execution(
        track: &mut TrackRuntime,
        planned: PlannedInventoryExecution,
    ) -> (Vec<DomainEvent>, Vec<TrackEffect>) {
        let PlannedInventoryExecution {
            events,
            effects,
            inventory_decision,
            new_runtime_state,
            execution_gate_decision,
            executor_state,
            fresh_acquisition_initialization,
            applied_risk_acquisition_release,
        } = planned;
        if let Some(new_runtime_state) = new_runtime_state {
            track.track_state = new_runtime_state;
        }
        track.execution_gate_state.last_decision = execution_gate_decision;
        track.apply_fresh_acquisition_initialization(fresh_acquisition_initialization);
        let desired_exposure = inventory_decision.risk_approved_target.clone();
        Self::apply_targeting_state(
            track,
            Some(desired_exposure),
            Some(inventory_decision),
            applied_risk_acquisition_release,
        );
        track.replace_executor_state(executor_state);

        (events, effects)
    }

    fn clear_targeting_state(track: &mut TrackRuntime) {
        Self::apply_targeting_state(track, None, None, None);
    }

    fn apply_targeting_state(
        track: &mut TrackRuntime,
        desired_exposure: Option<Exposure>,
        inventory_decision: Option<InventoryDecisionSnapshot>,
        applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
    ) {
        track.desired_exposure = desired_exposure;
        track.apply_inventory_decision_with_release(
            inventory_decision,
            applied_risk_acquisition_release,
        );
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

    fn submit_intent_input<'a>(
        &self,
        track: &'a TrackRuntime,
        risk_approved_target: poise_core::types::Exposure,
        execution_target: poise_core::types::Exposure,
        execution_gate_decision: ExecutionGateDecision,
        policy_context: executor::PolicyContext,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> executor::SubmitIntentInput<'a> {
        let submit_purpose = self.submit_purpose_for_track(track, &risk_approved_target);
        executor::SubmitIntentInput {
            instrument: track.instrument(),
            config: track.config(),
            instrument_spec: &track.instrument_spec,
            position_qty_per_unit: track.position_qty_per_exposure_unit(),
            min_rebalance_units: track.config().min_rebalance_units,
            current_exposure: track.current_exposure.clone(),
            risk_approved_target,
            execution_target,
            execution_gate_decision,
            execution_quote: Self::execution_quote_for_track(track),
            policy_context,
            price_execution_gate: track.price_execution_gate,
            submit_purpose,
            observed_at,
        }
    }

    fn policy_context_for_state(track_state: &TrackState) -> executor::PolicyContext {
        match track_state {
            TrackState::Running(ControlState::Manual(_)) => executor::PolicyContext::ManualOverride,
            TrackState::Running(ControlState::Automatic(
                AutoState::Frozen { .. }
                | AutoState::FlattenPending { .. }
                | AutoState::Flattening { .. },
            ))
            | TrackState::Terminated { .. } => executor::PolicyContext::ReduceOnly,
            _ => executor::PolicyContext::Normal,
        }
    }

    fn holds_protected_target(track_state: &TrackState) -> bool {
        matches!(
            track_state,
            TrackState::Running(ControlState::Automatic(
                AutoState::Frozen { .. } | AutoState::FlattenPending { .. }
            ))
        )
    }

    fn risk_exposure_gate_active(track: &TrackRuntime, next_state: &Option<TrackState>) -> bool {
        matches!(
            next_state.as_ref().unwrap_or(&track.track_state),
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated { .. }))
        )
    }

    fn plan_inventory_execution_for_track(
        &self,
        track: &TrackRuntime,
        strategy_price: f64,
    ) -> Result<PlannedInventoryExecution> {
        let normalized_track = self.track_with_current_pnl_day(track);
        let track = &normalized_track;
        let observed_at = self.clock.now();
        let reconciliation =
            reconciler::reconcile_target_at_with_metadata(track, strategy_price, observed_at);
        let applied_risk_acquisition_release = reconciliation.applied_risk_acquisition_release;
        let target = reconciliation.result;
        let fresh_acquisition_initialization = target.fresh_acquisition_initialization.clone();
        if track.executor_state.recovery_anomaly.is_some() {
            let reason = Self::select_primary_decision_reason(
                target.reason,
                Some(DecisionReason::RecoveryRequired),
            );
            let inventory_decision = Self::inventory_decision_snapshot(
                track,
                &target,
                strategy_price,
                observed_at,
                reason,
            );
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![],
                inventory_decision,
                new_runtime_state: target.new_runtime_state,
                execution_gate_decision: target.execution_gate_decision,
                executor_state: track.executor_state.clone(),
                fresh_acquisition_initialization,
                applied_risk_acquisition_release,
            });
        }
        let risk_exposure_gate_active =
            Self::risk_exposure_gate_active(track, &target.new_runtime_state);
        let execution_gate_blocks_submit = matches!(
            &target.execution_gate_decision,
            ExecutionGateDecision::NoSubmit { .. }
        );
        let effective_track_state = target
            .new_runtime_state
            .as_ref()
            .unwrap_or(&track.track_state);
        let policy_context = Self::policy_context_for_state(effective_track_state);
        let maintain_protected_bindings =
            target.target_pursuit_suppressed && Self::holds_protected_target(effective_track_state);
        let has_active_bindings = track
            .executor_state
            .bindings
            .iter()
            .any(|binding| binding.is_active());
        if target.target_pursuit_suppressed
            && !risk_exposure_gate_active
            && !execution_gate_blocks_submit
            && policy_context != executor::PolicyContext::ReduceOnly
            && !has_active_bindings
        {
            let executor_state = executor::refresh_state(
                &track.executor_state,
                track.config(),
                &track.current_exposure,
            );
            let reason = Self::select_primary_decision_reason(target.reason, None);
            let inventory_decision = Self::inventory_decision_snapshot(
                track,
                &target,
                strategy_price,
                observed_at,
                reason,
            );
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![],
                inventory_decision,
                new_runtime_state: target.new_runtime_state,
                execution_gate_decision: target.execution_gate_decision,
                executor_state,
                fresh_acquisition_initialization,
                applied_risk_acquisition_release,
            });
        }
        let executor_state = Some(&track.executor_state);
        let submit_intent = self.submit_intent_input(
            track,
            target.risk_approved_target.clone(),
            target.execution_target.clone(),
            target.execution_gate_decision.clone(),
            policy_context,
            observed_at,
        );
        let executor_input = executor::ExecutorInput::new(submit_intent, executor_state);
        let plan = if maintain_protected_bindings {
            executor::maintain_protected_bindings(executor_input)
        } else {
            executor::plan(executor_input)
        };
        let reason = Self::select_primary_decision_reason(target.reason, plan.reason);
        let inventory_decision =
            Self::inventory_decision_snapshot(track, &target, strategy_price, observed_at, reason);

        Ok(PlannedInventoryExecution {
            events: target.events,
            effects: plan.effects,
            inventory_decision,
            new_runtime_state: target.new_runtime_state,
            execution_gate_decision: target.execution_gate_decision,
            executor_state: plan.state,
            fresh_acquisition_initialization,
            applied_risk_acquisition_release,
        })
    }

    fn inventory_decision_snapshot(
        track: &TrackRuntime,
        target: &reconciler::TargetReconcileResult,
        strategy_price: f64,
        observed_at: chrono::DateTime<chrono::Utc>,
        reason: DecisionReason,
    ) -> InventoryDecisionSnapshot {
        InventoryDecisionSnapshot {
            observed_at,
            policy_target: target.policy_target.clone(),
            risk_approved_target: target.risk_approved_target.clone(),
            acquisition_frontier: target.acquisition_frontier.clone(),
            execution_target: target.execution_target.clone(),
            reason,
            applied_risk_cap: target.applied_risk_cap.clone(),
            price_execution_gate: track.price_execution_gate,
            revision: track.next_inventory_decision_revision(),
            risk_acquisition: track.risk_acquisition_runtime_view(
                target,
                strategy_price,
                observed_at,
            ),
        }
    }

    fn select_primary_decision_reason(
        target_reason: Option<DecisionReason>,
        executor_reason: Option<DecisionReason>,
    ) -> DecisionReason {
        match (target_reason, executor_reason) {
            (Some(target), Some(executor)) => {
                if decision_reason_priority(executor) > decision_reason_priority(target) {
                    executor
                } else {
                    target
                }
            }
            (Some(reason), None) | (None, Some(reason)) => reason,
            (None, None) => {
                unreachable!("inventory planning must provide a primary diagnostic reason")
            }
        }
    }

    fn current_pnl_utc_day(&self) -> chrono::NaiveDate {
        self.clock.now().date_naive()
    }

    fn track_with_current_pnl_day(&self, track: &TrackRuntime) -> TrackRuntime {
        let mut track = track.clone();
        track
            .pnl_stats
            .normalize_utc_day(self.current_pnl_utc_day());
        track
    }
}

fn decision_reason_priority(reason: DecisionReason) -> u8 {
    match reason {
        DecisionReason::RecoveryRequired => 100,
        DecisionReason::RiskInputInvalid => 99,
        DecisionReason::AccountCapacityBlocked => 90,
        DecisionReason::SubmitPlanned => 80,
        DecisionReason::BandPolicyBlocked
        | DecisionReason::PriceGateBlocked
        | DecisionReason::WaitingCancel => 70,
        DecisionReason::WaitingRiskRelease => 60,
        DecisionReason::CoveredByWorkingOrder => 50,
        DecisionReason::BelowVenueMinimum => 40,
        DecisionReason::BelowRebalance => 30,
        DecisionReason::AtExecutionTarget => 20,
        DecisionReason::WaitingMarketData | DecisionReason::Paused => 10,
    }
}

struct PlannedInventoryExecution {
    events: Vec<DomainEvent>,
    effects: Vec<TrackEffect>,
    inventory_decision: InventoryDecisionSnapshot,
    new_runtime_state: Option<TrackState>,
    execution_gate_decision: ExecutionGateDecision,
    executor_state: ExecutorState,
    fresh_acquisition_initialization: Option<FreshAcquisitionInitialization>,
    applied_risk_acquisition_release: Option<AppliedRiskAcquisitionRelease>,
}

fn market_mutation_requires_durable_write(
    previous_state: &TrackRuntime,
    next_state: &TrackRuntime,
    events: &[DomainEvent],
    effects: &[TrackEffect],
) -> bool {
    let desired_exposure_changed = previous_state.desired_exposure != next_state.desired_exposure;
    let has_non_target_events = events
        .iter()
        .any(|event| !matches!(event, DomainEvent::ExposureTargetChanged { .. }));
    let has_execution_effects = !effects.is_empty();
    let durable_state_changed_without_target = {
        let mut normalized_next = next_state.clone();
        normalized_next.desired_exposure = previous_state.desired_exposure.clone();
        normalized_next.inventory_decision = previous_state.inventory_decision.clone();
        normalized_next.inventory_decision_revision = previous_state.inventory_decision_revision;
        normalized_next.applied_risk_acquisition_release =
            previous_state.applied_risk_acquisition_release.clone();
        normalized_next.fresh_acquisition_session =
            previous_state.fresh_acquisition_session.clone();
        if !has_non_target_events && !has_execution_effects {
            normalized_next.executor_state = previous_state.executor_state.clone();
        }
        normalized_next.pnl_stats = previous_state.pnl_stats.clone();
        normalized_next.strategy_price = previous_state.strategy_price;
        normalized_next.strategy_price_status = previous_state.strategy_price_status;
        normalized_next.mark_price = previous_state.mark_price;
        normalized_next.best_bid = previous_state.best_bid;
        normalized_next.best_ask = previous_state.best_ask;
        normalized_next.price_execution_gate = previous_state.price_execution_gate;
        normalized_next.last_tick_at = previous_state.last_tick_at;
        normalized_next
            .set_exchange_observation_revision(previous_state.exchange_observation_revision());
        normalized_next != *previous_state
    };
    let target_reached_without_new_effects = next_state
        .desired_exposure
        .as_ref()
        .is_some_and(|target| previous_state.current_exposure.delta(target).is_zero());
    let durable_target_change = desired_exposure_changed
        && (has_execution_effects
            || has_non_target_events
            || target_reached_without_new_effects
            || next_state.desired_exposure.is_none());

    durable_state_changed_without_target
        || has_non_target_events
        || has_execution_effects
        || durable_target_change
}

fn retain_live_only_executor_state(
    track: &mut TrackRuntime,
    executor_state: ExecutorState,
    executor_view_changed: bool,
) {
    if executor_view_changed {
        track.replace_executor_state(executor_state);
    } else {
        // Market planning may legitimately advance private ledger/timer state. Keep that
        // state without invalidating an exchange snapshot whose binding interpretation is
        // unchanged.
        track.executor_state = executor_state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use poise_core::events::ExecutionGateReason;
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{
        BandProtectionPolicy, RiskAcquisitionConfig, ShapeFamily, TrackConfig,
    };
    use poise_core::track::TrackDefinition;
    use poise_core::types::Side;

    use crate::exchange::ExecutionQuote;
    use crate::execution_plan::TrackEffect;
    use crate::executor::binding::LiveOrderBinding;
    use crate::executor::{PolicyContext, policy::PolicyKind};
    use crate::ledger::TrackPnlStats;
    use crate::price_gate::{PriceExecutionBlockReason, PriceExecutionGate};
    use crate::risk_exposure_gate::RiskExposureGateState;
    use crate::runtime::{CurrentMarketData, TrackStatus};
    use poise_core::track::Venue;

    fn amount(value: f64, asset: &str) -> Amount {
        Amount::new(value, Asset::try_new(asset).unwrap()).unwrap()
    }

    #[derive(Debug)]
    struct FixedClock(chrono::DateTime<Utc>);

    impl ClockPort for FixedClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            self.0
        }
    }

    fn manager() -> (TrackManager, TrackId) {
        let mut manager = TrackManager::new(Arc::new(FixedClock(
            Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap(),
        )));
        let id = TrackId::from("test");
        let definition = TrackDefinition::try_new(
            id.clone(),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            config(),
            Some(10_000.0),
            loss_limits(),
            None,
        )
        .unwrap();
        manager.add_track(definition, rules()).unwrap();
        (manager, id)
    }

    fn config() -> TrackConfig {
        TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 100.0,
            min_rebalance_units: 1.0,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: BandProtectionPolicy::Freeze,
            risk_acquisition: Default::default(),
        }
    }

    fn loss_limits() -> LossLimits {
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        }
    }

    fn rules() -> InstrumentSpec {
        crate::test_instrument::linear_spec(0.1, 0.01, 0.0, 0.0)
    }

    fn inverse_rules() -> InstrumentSpec {
        crate::test_instrument::inverse_spec(0.1, 1.0, 1.0, 0.0, 100.0)
    }

    fn market(price: f64) -> TrackObservation {
        TrackObservation::Market(MarketObservation::ExecutionQuote {
            execution_quote: ExecutionQuote {
                best_bid: price - 0.1,
                best_ask: price + 0.1,
            },
        })
    }

    #[test]
    fn reconcile_track_submits_catch_up_action_from_due_boundary_operation() {
        let (mut manager, id) = manager();

        let transition = manager.observe(&id, market(95.0)).unwrap();
        let track = manager.tracks.get(&id).unwrap();
        let catch_up_binding = track
            .executor_state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be tracked");
        assert_eq!(
            track.inventory_decision().unwrap().reason,
            crate::execution_plan::DecisionReason::SubmitPlanned
        );
        assert!(!transition.effects.is_empty());

        let request = transition
            .effects
            .iter()
            .find_map(|effect| match effect {
                TrackEffect::SubmitOrder { request, .. }
                    if request.client_order_id == catch_up_binding.request.client_order_id =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("catch-up submit effect should exist");
        assert_eq!(request.side, Side::Buy);
        assert!((request.price - 95.1).abs() < 1e-9);
        assert!((request.quantity - 1.0).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 1);
    }

    #[test]
    fn manager_passes_reconciled_final_target_to_executor() {
        let (mut manager, id) = manager();

        manager.observe(&id, market(93.75)).unwrap();
        let track = manager.tracks.get(&id).unwrap();
        let catch_up_binding = track
            .executor_state
            .bindings
            .iter()
            .find(|binding| binding.proposal_key.policy == PolicyKind::CatchUp)
            .expect("catch-up binding should be tracked");

        assert_eq!(track.desired_exposure, Some(Exposure(5.0)));
        assert_eq!(catch_up_binding.desired_exposure, Exposure(2.5));
    }

    #[test]
    fn runtime_view_projects_the_committed_inventory_decision() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();

        let committed = manager.track_runtime_view(&id).unwrap().decision;
        assert_eq!(committed.revision, Some(1));

        let track = manager.tracks.get_mut(&id).unwrap();
        track.strategy_price = Some(105.0);

        let queried_twice = [
            manager.track_runtime_view(&id).unwrap(),
            manager.track_runtime_view(&id).unwrap(),
        ];
        for view in queried_twice {
            assert_eq!(view.decision, committed);
        }
    }

    #[test]
    fn committed_inventory_decision_keeps_policy_target_before_risk_cap() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.replace_definition_for_test(config(), 400.0, loss_limits());
        }

        manager.observe(&id, market(90.0)).unwrap();
        let decision = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .unwrap();

        assert_eq!(decision.policy_target, Exposure(8.0));
        assert_eq!(decision.risk_approved_target, Exposure(4.0));
        assert_eq!(
            decision.applied_risk_cap,
            Some(crate::runtime::AppliedRiskCap {
                intended: Exposure(8.0),
                capped: Exposure(4.0),
            })
        );
    }

    #[test]
    fn decision_revision_remains_monotonic_across_pause_and_resume() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();
        let first_revision = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .unwrap()
            .revision;
        let owned_executor_state = manager
            .get_track(id.as_str())
            .unwrap()
            .executor_state
            .clone();

        manager.pause_track(id.as_str()).unwrap();
        assert!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .is_none()
        );
        manager.resume_track(id.as_str()).unwrap();
        assert!(matches!(
            manager.get_track(id.as_str()).unwrap().track_state,
            TrackState::Running(ControlState::Automatic(AutoState::FollowingBand))
        ));
        assert_eq!(
            manager.get_track(id.as_str()).unwrap().executor_state,
            owned_executor_state,
            "pause/resume must not drop current-session order ownership"
        );
        assert!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .is_none(),
            "resume must wait for the next fully owned reconcile"
        );
        let transition = manager.observe(&id, market(95.0)).unwrap();

        assert_eq!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .unwrap()
                .revision,
            first_revision + 1
        );
        assert!(!transition.events.is_empty());
    }

    #[test]
    fn resume_preserves_suspended_manual_control_without_market_data() {
        let (mut manager, id) = manager();
        manager.tracks.get_mut(&id).unwrap().track_state = TrackState::Paused {
            suspended: ControlState::Manual(ManualState::TargetOverride {
                target: Exposure(3.0),
            }),
        };

        let (events, effects) = manager.resume_track(id.as_str()).unwrap();

        assert!(events.is_empty());
        assert!(effects.is_empty());
        assert!(matches!(
            manager.get_track(id.as_str()).unwrap().track_state,
            TrackState::Running(ControlState::Manual(ManualState::TargetOverride {
                target: Exposure(3.0)
            }))
        ));
        assert!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .is_none()
        );

        manager.observe(&id, market(100.0)).unwrap();
        assert_eq!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .unwrap()
                .risk_approved_target,
            Exposure(3.0)
        );
    }

    #[test]
    fn live_only_market_mutation_commits_snapshot_without_durable_transition() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();
        let previous = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .cloned()
            .unwrap();

        let outcome = manager
            .observe_market_mutation(
                &id,
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 94.9,
                        best_ask: 95.1,
                    },
                },
            )
            .unwrap();

        assert!(matches!(outcome, MarketMutationOutcome::LiveOnly));
        let next = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .unwrap();
        assert_eq!(next.revision, previous.revision + 1);
        assert_eq!(next.risk_approved_target, previous.risk_approved_target);
        assert_eq!(next.execution_target, previous.execution_target);
    }

    #[test]
    fn market_mutation_keeps_fresh_acquisition_initialization() {
        let (mut manager, id) = manager();
        manager
            .fresh_start_track(
                &id,
                TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
                TrackPnlStats::default(),
                FreshSessionExternalInputs {
                    current_exposure: Exposure(2.0),
                    position_qty: 2.0,
                    market_data: None,
                    instrument_spec: rules(),
                },
            )
            .unwrap();

        let outcome = manager
            .observe_market_mutation(
                &id,
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 92.4,
                        best_ask: 92.6,
                    },
                },
            )
            .unwrap();

        assert!(matches!(outcome, MarketMutationOutcome::Durable(_)));
        let track = manager.get_track(id.as_str()).unwrap();
        assert_eq!(
            track.inventory_decision().unwrap().execution_target,
            Exposure(2.0)
        );
        assert!(!track.fresh_acquisition_pending());
        assert_eq!(
            track.acquisition_session_view().unwrap().initial_frontier,
            Some(Exposure(2.0))
        );
    }

    #[test]
    fn live_only_market_mutation_keeps_executor_maintenance_state() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(100.0)).unwrap();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.executor_state.bindings.push(LiveOrderBinding {
                binding_id: "queued-increase-binding".to_string(),
                proposal_key: crate::executor::binding::BindingProposalKey {
                    policy: PolicyKind::CatchUp,
                    operations: Vec::new(),
                },
                allocations: Vec::new(),
                absorbed_exposure_qty: 0.0,
                request: crate::exchange::OrderRequest {
                    instrument: track.instrument().clone(),
                    side: Side::Buy,
                    price: 100.1,
                    quantity: 1.0,
                    client_order_id: "queued-increase-client".to_string(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(1.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
                order_id: None,
                status: crate::executor::binding::BindingStatus::SubmitPending,
                policy_state: crate::executor::binding::BindingPolicyState::Stateless,
            });
            let mut authorized = track.executor_state.bindings[0].clone();
            authorized.binding_id = "authorized-increase-binding".to_string();
            authorized.request.client_order_id = "authorized-increase-client".to_string();
            authorized.status = crate::executor::binding::BindingStatus::SubmitAuthorized;
            track.executor_state.bindings.push(authorized);
        }
        let previous_exchange_revision = manager
            .get_track(id.as_str())
            .unwrap()
            .exchange_observation_revision();

        let outcome = manager
            .observe_market_mutation(&id, MarketObservation::MarkPrice { mark_price: 200.0 })
            .unwrap();

        assert!(matches!(outcome, MarketMutationOutcome::LiveOnly));
        let bindings = &manager
            .get_track(id.as_str())
            .unwrap()
            .executor_state
            .bindings;
        assert_eq!(bindings.len(), 1);
        assert_eq!(
            bindings[0].status,
            crate::executor::binding::BindingStatus::CancelAfterSubmit,
            "live-only mutation must retain queued terminalization and in-flight protection"
        );
        assert_ne!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .exchange_observation_revision(),
            previous_exchange_revision,
            "binding interpretation changes must invalidate older exchange snapshots"
        );
    }

    #[test]
    fn live_only_private_executor_change_does_not_advance_exchange_revision() {
        let (mut manager, id) = manager();
        let track = manager.tracks.get_mut(&id).unwrap();
        let previous_revision = track.exchange_observation_revision();
        let previous_state = track.clone();
        let mut executor_state = track.executor_state.clone();
        executor_state.ledger_state.ledger_anchor_exposure = Exposure(2.0);
        let mut next_state = previous_state.clone();
        next_state.executor_state = executor_state.clone();
        let executor_view_changed = previous_state.executor_view_changed(&next_state);

        assert!(!executor_view_changed);

        retain_live_only_executor_state(track, executor_state.clone(), executor_view_changed);

        assert_eq!(track.executor_state, executor_state);
        assert_eq!(track.exchange_observation_revision(), previous_revision);
    }

    #[test]
    fn live_only_target_change_keeps_operational_target_in_sync_with_snapshot() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();
        let previous = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .cloned()
            .unwrap();

        let outcome = manager
            .observe_market_mutation(
                &id,
                MarketObservation::ExecutionQuote {
                    execution_quote: ExecutionQuote {
                        best_bid: 96.15,
                        best_ask: 96.35,
                    },
                },
            )
            .unwrap();

        assert!(matches!(outcome, MarketMutationOutcome::LiveOnly));
        let track = manager.get_track(id.as_str()).unwrap();
        let decision = track.inventory_decision().unwrap();
        assert_ne!(decision.risk_approved_target, previous.risk_approved_target);
        assert_eq!(decision.execution_target, previous.execution_target);
        assert_eq!(
            track.desired_exposure,
            Some(decision.risk_approved_target.clone())
        );
    }

    #[test]
    fn installing_a_previous_candidate_restores_its_inventory_decision() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();
        let previous_state = manager.get_track(id.as_str()).cloned().unwrap();
        let previous_decision = manager
            .get_track(id.as_str())
            .unwrap()
            .inventory_decision()
            .cloned();

        manager.observe(&id, market(105.0)).unwrap();
        assert_ne!(
            manager.get_track(id.as_str()).unwrap().inventory_decision(),
            previous_decision.as_ref()
        );

        manager.install_planned_runtime(previous_state);
        assert_eq!(
            manager.get_track(id.as_str()).unwrap().inventory_decision(),
            previous_decision.as_ref()
        );
    }

    #[test]
    fn reconcile_track_does_not_plan_risk_acquisition_maker_from_next_release() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            let mut config = track.config().clone();
            config.min_rebalance_units = 0.5;
            config.risk_acquisition = RiskAcquisitionConfig::default();
            track.replace_definition_for_test(
                config,
                track.max_notional(),
                track.loss_limits().clone(),
            );
            track.current_exposure = Exposure(1.5);
            track.desired_exposure = Some(Exposure(1.5));
            track.track_state =
                TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                    gate: RiskExposureGateState {
                        risk_release_frontier: Exposure(1.5),
                        release_anchor_price: 93.75,
                        release_anchor_target: Exposure(5.0),
                        stale_since: chrono::Utc::now(),
                    },
                }));
            track.executor_state =
                ExecutorState::empty(Utc.with_ymd_and_hms(2026, 4, 22, 8, 0, 0).unwrap())
                    .ensure_revision(track.config(), track.current_exposure.clone());
        }

        let transition = manager.observe(&id, market(93.75)).unwrap();

        assert!(!transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. }
                if request.side == Side::Buy && (request.price - 92.5).abs() < 1e-9
        )));
        assert!(transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. }
                if request.side == Side::Sell && request.reduce_only
        )));
    }

    #[test]
    fn stale_risk_release_submits_catch_up_even_when_previous_release_is_unfilled() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.current_exposure = Exposure(-0.75);
            track.desired_exposure = Some(Exposure(-4.0));
            track.track_state =
                TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                    gate: RiskExposureGateState {
                        risk_release_frontier: Exposure(-1.5),
                        release_anchor_price: 105.0,
                        release_anchor_target: Exposure(-4.0),
                        stale_since: Utc.with_ymd_and_hms(2026, 4, 22, 7, 59, 0).unwrap(),
                    },
                }));
        }

        let transition = manager.observe(&id, market(105.625)).unwrap();
        let track = manager.tracks.get(&id).unwrap();

        assert!(transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. }
                if request.side == Side::Sell && request.client_order_id.starts_with("bc")
        )));
        assert!(track.executor_state.bindings.iter().any(|binding| {
            binding.proposal_key.policy == PolicyKind::CatchUp && binding.request.side == Side::Sell
        }));
        assert!(matches!(
            &track.track_state,
            TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                gate
            })) if gate.risk_release_frontier == Exposure(-2.5)
        ));
    }

    #[test]
    fn account_capacity_block_is_not_bypassed_by_active_acquisition_gate() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.current_exposure = Exposure(1.5);
            track.desired_exposure = Some(Exposure(1.5));
            track.track_state =
                TrackState::Running(ControlState::Automatic(AutoState::RiskExposureGated {
                    gate: RiskExposureGateState {
                        risk_release_frontier: Exposure(1.5),
                        release_anchor_price: 93.75,
                        release_anchor_target: Exposure(5.0),
                        stale_since: Utc.with_ymd_and_hms(2026, 4, 22, 8, 0, 0).unwrap(),
                    },
                }));
            track
                .execution_gate_state
                .account_capacity
                .available_notional = Some(0.0);
            track.executor_state =
                ExecutorState::empty(Utc.with_ymd_and_hms(2026, 4, 22, 8, 0, 0).unwrap())
                    .ensure_revision(track.config(), track.current_exposure.clone());
        }

        let transition = manager.observe(&id, market(93.75)).unwrap();

        assert!(transition.events.iter().any(|event| matches!(
            event,
            DomainEvent::ExecutionGateApplied {
                reason: ExecutionGateReason::AccountCapacityInsufficient { .. }
            }
        )));
        assert!(!transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. } if !request.reduce_only
        )));
        assert_eq!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .unwrap()
                .reason,
            DecisionReason::AccountCapacityBlocked
        );
    }

    #[test]
    fn protected_state_transition_does_not_pursue_a_risk_increasing_target() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.current_exposure = Exposure(1.0);
            track.desired_exposure = Some(Exposure(4.0));
            track.track_state =
                TrackState::Running(ControlState::Automatic(AutoState::FollowingBand));
            track.executor_state =
                ExecutorState::empty(Utc.with_ymd_and_hms(2026, 4, 22, 8, 0, 0).unwrap())
                    .ensure_revision(track.config(), track.current_exposure.clone());
            track.executor_state.bindings.push(LiveOrderBinding {
                binding_id: "protected-increase-binding".to_string(),
                proposal_key: crate::executor::binding::BindingProposalKey {
                    policy: PolicyKind::CatchUp,
                    operations: Vec::new(),
                },
                allocations: Vec::new(),
                absorbed_exposure_qty: 0.0,
                request: crate::exchange::OrderRequest {
                    instrument: track.instrument().clone(),
                    side: Side::Buy,
                    price: 85.1,
                    quantity: 1.0,
                    client_order_id: "protected-increase-client".to_string(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
                order_id: Some("protected-increase-order".to_string()),
                status: crate::executor::binding::BindingStatus::Working,
                policy_state: crate::executor::binding::BindingPolicyState::Stateless,
            });
            track.executor_state.bindings.push(LiveOrderBinding {
                binding_id: "protected-reduce-only-binding".to_string(),
                proposal_key: crate::executor::binding::BindingProposalKey {
                    policy: PolicyKind::CurveMaker,
                    operations: Vec::new(),
                },
                allocations: Vec::new(),
                absorbed_exposure_qty: 0.0,
                request: crate::exchange::OrderRequest {
                    instrument: track.instrument().clone(),
                    side: Side::Sell,
                    price: 94.9,
                    quantity: 0.5,
                    client_order_id: "protected-reduce-only-client".to_string(),
                    reduce_only: true,
                },
                desired_exposure: Exposure(0.5),
                submit_purpose: SubmitPurpose::AutoReconcile,
                order_id: Some("protected-reduce-only-order".to_string()),
                status: crate::executor::binding::BindingStatus::Working,
                policy_state: crate::executor::binding::BindingPolicyState::CurveMaker {
                    due_grace_started_at: None,
                },
            });
        }

        let transition = manager.observe(&id, market(85.0)).unwrap();
        let track = manager.tracks.get(&id).unwrap();

        assert!(matches!(
            track.track_state,
            TrackState::Running(ControlState::Automatic(AutoState::Frozen { .. }))
        ));
        assert!(transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "protected-increase-order"
        )));
        assert!(track.executor_state.bindings.iter().any(|binding| {
            binding.request.client_order_id == "protected-increase-client"
                && binding.status == crate::executor::binding::BindingStatus::CancelPending
        }));
        assert!(track.executor_state.bindings.iter().any(|binding| {
            binding.request.client_order_id == "protected-reduce-only-client"
                && binding.status == crate::executor::binding::BindingStatus::Working
        }));
        assert!(!transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "protected-reduce-only-order"
        )));
        assert!(!transition.effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::SubmitOrder { request, .. } if !request.reduce_only
        )));
    }

    #[test]
    fn reconcile_track_reopens_boundary_ledger_when_profile_revision_changes() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(95.0)).unwrap();
        let old_revision = manager
            .tracks
            .get(&id)
            .unwrap()
            .executor_state
            .ledger_state
            .profile_revision
            .clone();

        {
            let track = manager.tracks.get_mut(&id).unwrap();
            let mut config = track.config().clone();
            config.notional_per_unit = 120.0;
            track.replace_definition_for_test(
                config,
                track.max_notional(),
                track.loss_limits().clone(),
            );
        }

        manager.observe(&id, market(100.0)).unwrap();

        let track = manager.tracks.get(&id).unwrap();
        assert_ne!(
            track.executor_state.ledger_state.profile_revision,
            old_revision
        );
        assert_eq!(
            track.executor_state.ledger_state.ledger_anchor_exposure,
            Exposure(0.0)
        );
        assert!(track.executor_state.bindings.is_empty());
    }

    #[test]
    fn manager_maps_manual_track_state_to_manual_override_policy_context() {
        let track_state = TrackState::Running(ControlState::Manual(ManualState::Flattened));

        assert_eq!(
            TrackManager::policy_context_for_state(&track_state),
            PolicyContext::ManualOverride
        );
    }

    #[test]
    fn manager_maps_protected_track_states_to_reduce_only_policy_context() {
        let cases = vec![
            TrackState::Running(ControlState::Automatic(AutoState::Frozen {
                target_anchor: Exposure(0.0),
            })),
            TrackState::Running(ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(0.0),
                boundary: poise_core::strategy::BandBoundary::Below,
            })),
            TrackState::Running(ControlState::Automatic(AutoState::Flattening {
                boundary: poise_core::strategy::BandBoundary::Above,
            })),
            TrackState::Terminated {
                cause: TerminationCause::ManualCommand,
            },
        ];

        for track_state in cases {
            assert_eq!(
                TrackManager::policy_context_for_state(&track_state),
                PolicyContext::ReduceOnly
            );
        }
    }

    #[test]
    fn manager_selects_one_primary_reason_without_changing_planning_control_flow() {
        let cases = [
            (
                Some(DecisionReason::RiskInputInvalid),
                Some(DecisionReason::SubmitPlanned),
                DecisionReason::RiskInputInvalid,
            ),
            (
                Some(DecisionReason::AccountCapacityBlocked),
                Some(DecisionReason::WaitingCancel),
                DecisionReason::AccountCapacityBlocked,
            ),
            (
                Some(DecisionReason::WaitingRiskRelease),
                Some(DecisionReason::CoveredByWorkingOrder),
                DecisionReason::WaitingRiskRelease,
            ),
            (
                None,
                Some(DecisionReason::SubmitPlanned),
                DecisionReason::SubmitPlanned,
            ),
        ];

        for (target, executor, expected) in cases {
            assert_eq!(
                TrackManager::select_primary_decision_reason(target, executor),
                expected
            );
        }
    }

    #[test]
    fn price_gate_still_cancels_increasing_binding_at_execution_target() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(100.0)).unwrap();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.price_execution_gate = PriceExecutionGate::NoSubmit {
                reason: PriceExecutionBlockReason::MarkBookDivergence,
            };
            track.executor_state.bindings.push(LiveOrderBinding {
                binding_id: "at-target-increase-binding".to_string(),
                proposal_key: crate::executor::binding::BindingProposalKey {
                    policy: PolicyKind::CatchUp,
                    operations: Vec::new(),
                },
                allocations: Vec::new(),
                absorbed_exposure_qty: 0.0,
                request: crate::exchange::OrderRequest {
                    instrument: track.instrument().clone(),
                    side: Side::Buy,
                    price: 99.9,
                    quantity: 1.0,
                    client_order_id: "at-target-increase-client".to_string(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(1.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
                order_id: Some("at-target-increase-order".to_string()),
                status: crate::executor::binding::BindingStatus::Working,
                policy_state: crate::executor::binding::BindingPolicyState::Stateless,
            });
        }

        let (_, effects) = manager.reconcile_track(&id, 100.0).unwrap();

        assert!(effects.iter().any(|effect| matches!(
            effect,
            TrackEffect::CancelOrder { order_id, .. }
                if order_id == "at-target-increase-order"
        )));
        assert_eq!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .unwrap()
                .reason,
            DecisionReason::PriceGateBlocked
        );
    }

    #[test]
    fn terminated_track_at_zero_reports_at_execution_target() {
        let (mut manager, id) = manager();
        manager.observe(&id, market(100.0)).unwrap();

        let transition = manager.command(&id, TrackCommand::Terminate).unwrap();

        assert!(transition.effects.is_empty());
        assert_eq!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .inventory_decision()
                .unwrap()
                .reason,
            DecisionReason::AtExecutionTarget
        );
    }

    #[test]
    fn manager_uses_maintenance_only_for_target_holding_protected_states() {
        assert!(TrackManager::holds_protected_target(&TrackState::Running(
            ControlState::Automatic(AutoState::Frozen {
                target_anchor: Exposure(1.0),
            })
        )));
        assert!(TrackManager::holds_protected_target(&TrackState::Running(
            ControlState::Automatic(AutoState::FlattenPending {
                target_anchor: Exposure(1.0),
                boundary: poise_core::strategy::BandBoundary::Below,
            })
        )));
        assert!(!TrackManager::holds_protected_target(&TrackState::Running(
            ControlState::Automatic(AutoState::Flattening {
                boundary: poise_core::strategy::BandBoundary::Below,
            })
        )));
        assert!(!TrackManager::holds_protected_target(
            &TrackState::Terminated {
                cause: TerminationCause::ManualCommand,
            }
        ));
    }

    #[test]
    fn position_observation_does_not_mutate_pnl_stats() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.pnl_stats.gross_realized_pnl_today = 20.0;
            track.pnl_stats.gross_realized_pnl_cumulative = 20.0;
        }

        manager
            .observe(
                &id,
                TrackObservation::Position(PositionObservation {
                    qty: -0.2,
                    unrealized_pnl: 5.0,
                }),
            )
            .unwrap();

        let track = manager.tracks.get(&id).unwrap();
        assert_eq!(track.current_position_qty, -0.2);
        assert_eq!(track.current_exposure, Exposure(-0.2));
        assert!((track.risk_state.unrealized_pnl - 5.0).abs() < f64::EPSILON);
        assert!((track.pnl_stats.gross_realized_pnl_today - 20.0).abs() < f64::EPSILON);
        assert!((track.pnl_stats.gross_realized_pnl_cumulative - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn position_observation_uses_inverse_contract_quantity_for_exposure() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            let mut config = track.config().clone();
            config.lower_price = 90_000.0;
            config.upper_price = 110_000.0;
            config.notional_per_unit = 1_000.0;
            track.replace_definition_for_test(
                config,
                track.max_notional(),
                track.loss_limits().clone(),
            );
            track.instrument_spec = inverse_rules();
        }

        manager
            .observe(
                &id,
                TrackObservation::Position(PositionObservation {
                    qty: -30.0,
                    unrealized_pnl: 0.25,
                }),
            )
            .unwrap();

        let track = manager.tracks.get(&id).unwrap();
        assert_eq!(track.current_position_qty, -30.0);
        assert_eq!(track.current_exposure, Exposure(-3.0));
    }

    #[test]
    fn applying_pnl_record_rolls_today_window_to_current_utc_day() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
            track.pnl_stats.gross_realized_pnl_today = 20.0;
            track.pnl_stats.trading_fee_today = 2.0;
            track.pnl_stats.gross_realized_pnl_cumulative = 20.0;
            track.pnl_stats.trading_fee_cumulative = 2.0;
        }

        manager
            .apply_pnl_record(
                &id,
                &TrackPnlRecord::trade_summary(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap(),
                    "test".into(),
                    None,
                    None,
                    amount(5.0, "USDT"),
                    Some(amount(0.5, "USDT")),
                ),
            )
            .unwrap();

        let stats = &manager.tracks.get(&id).unwrap().pnl_stats;
        assert_eq!(
            stats.pnl_utc_day,
            chrono::NaiveDate::from_ymd_opt(2026, 4, 22).unwrap()
        );
        assert_eq!(stats.gross_realized_pnl_today, 5.0);
        assert_eq!(stats.trading_fee_today, 0.5);
        assert_eq!(stats.gross_realized_pnl_cumulative, 25.0);
        assert_eq!(stats.trading_fee_cumulative, 2.5);
    }

    #[test]
    fn applying_pnl_record_preserves_mismatched_asset_as_unvalued_cashflow() {
        let (mut manager, id) = manager();

        manager
            .apply_pnl_record(
                &id,
                &TrackPnlRecord::trade_summary(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap(),
                    "test:usdt".into(),
                    None,
                    None,
                    amount(5.0, "USDT"),
                    Some(amount(0.5, "USDT")),
                ),
            )
            .unwrap();
        manager
            .apply_pnl_record(
                &id,
                &TrackPnlRecord::funding(
                    Instrument::new(Venue::Binance, "BTCUSDT"),
                    Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap(),
                    "test:btc".into(),
                    Some("test:btc:1".into()),
                    amount(0.001, "BTC"),
                ),
            )
            .unwrap();

        let stats = &manager.tracks.get(&id).unwrap().pnl_stats;
        assert_eq!(stats.pnl_asset.as_ref().unwrap().as_str(), "USDT");
        assert_eq!(stats.gross_realized_pnl_cumulative, 5.0);
        assert_eq!(stats.trading_fee_cumulative, 0.5);
        let aggregate = stats.net_realized_pnl_cumulative();
        assert_eq!(aggregate.valued_amount, 4.5);
        assert!(aggregate.complete_value().is_none());
        assert_eq!(stats.unvalued_cashflows_cumulative.len(), 1);
        assert_eq!(
            stats.unvalued_cashflows_cumulative[0]
                .amount
                .as_ref()
                .unwrap()
                .asset()
                .as_str(),
            "BTC"
        );
    }

    #[test]
    fn runtime_view_rolls_today_window_even_without_new_pnl_record() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.pnl_stats.pnl_utc_day = chrono::NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
            track.pnl_stats.gross_realized_pnl_today = 20.0;
            track.pnl_stats.trading_fee_today = 2.0;
            track.pnl_stats.gross_realized_pnl_cumulative = 20.0;
            track.pnl_stats.trading_fee_cumulative = 2.0;
        }

        let view = manager.track_runtime_view(&id).unwrap();

        assert_eq!(
            view.pnl_stats.pnl_utc_day,
            chrono::NaiveDate::from_ymd_opt(2026, 4, 22).unwrap()
        );
        assert_eq!(view.pnl_stats.gross_realized_pnl_today, 0.0);
        assert_eq!(view.pnl_stats.trading_fee_today, 0.0);
        assert_eq!(view.pnl_stats.gross_realized_pnl_cumulative, 20.0);
        assert_eq!(view.pnl_stats.trading_fee_cumulative, 2.0);
    }

    #[test]
    fn fresh_start_track_replaces_existing_runtime_with_new_session_state() {
        let (mut manager, id) = manager();
        {
            let track = manager.tracks.get_mut(&id).unwrap();
            track.current_exposure = Exposure(9.0);
            track.desired_exposure = Some(Exposure(4.0));
            track.executor_state.bindings.push(LiveOrderBinding {
                binding_id: "binding-1".into(),
                proposal_key: crate::executor::binding::BindingProposalKey {
                    policy: PolicyKind::CatchUp,
                    operations: Vec::new(),
                },
                allocations: Vec::new(),
                absorbed_exposure_qty: 0.0,
                request: crate::exchange::OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 1.0,
                    client_order_id: "client-1".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(1.0),
                submit_purpose: crate::price_gate::SubmitPurpose::AutoReconcile,
                order_id: None,
                status: crate::executor::binding::BindingStatus::Working,
                policy_state: crate::executor::binding::BindingPolicyState::Stateless,
            });
            track.executor_state.recovery_anomaly =
                Some(crate::executor::RecoveryAnomaly::UnknownLiveOrder);
            track.strategy_price = Some(100.0);
            track.strategy_price_status = StrategyPriceStatus::Live;
            track.best_bid = Some(99.9);
            track.best_ask = Some(100.1);
        }

        manager
            .fresh_start_track(
                &id,
                TrackState::Paused {
                    suspended: ControlState::Automatic(AutoState::FollowingBand),
                },
                TrackPnlStats {
                    gross_realized_pnl_cumulative: 13.0,
                    ..TrackPnlStats::default()
                },
                FreshSessionExternalInputs {
                    current_exposure: Exposure(1.5),
                    position_qty: 1.5,
                    market_data: Some(CurrentMarketData {
                        strategy_price: 96.0,
                        mark_price: Some(95.9),
                        execution_quote: ExecutionQuote {
                            best_bid: 95.8,
                            best_ask: 96.2,
                        },
                        observed_at: Utc.with_ymd_and_hms(2026, 4, 22, 9, 1, 0).unwrap(),
                    }),
                    instrument_spec: crate::test_instrument::linear_spec(0.5, 0.01, 0.0, 0.0),
                },
            )
            .unwrap();

        let track = manager.get_track(id.as_str()).unwrap();
        assert_eq!(track.status(), TrackStatus::Paused);
        assert_eq!(track.current_exposure, Exposure(1.5));
        assert_eq!(track.desired_exposure, None);
        assert!(track.executor_state.bindings.is_empty());
        assert!(track.executor_state.recovery_anomaly.is_none());
        assert_eq!(track.pnl_stats.gross_realized_pnl_cumulative, 13.0);
        assert_eq!(track.strategy_price, Some(96.0));
        assert_eq!(track.best_bid, Some(95.8));
        assert_eq!(track.best_ask, Some(96.2));
        assert!(track.inventory_decision().is_none());
        assert!(track.fresh_acquisition_pending());
    }

    #[test]
    fn manager_installs_fresh_acquisition_initialization_once() {
        let (mut manager, id) = manager();
        manager
            .fresh_start_track(
                &id,
                TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
                TrackPnlStats::default(),
                FreshSessionExternalInputs {
                    current_exposure: Exposure(2.0),
                    position_qty: 2.0,
                    market_data: None,
                    instrument_spec: rules(),
                },
            )
            .unwrap();

        manager.observe(&id, market(92.5)).unwrap();

        let track = manager.get_track(id.as_str()).unwrap();
        let decision = track.inventory_decision().unwrap();
        assert_eq!(decision.risk_approved_target, Exposure(6.0));
        assert_eq!(decision.execution_target, Exposure(2.0));
        assert!(!track.fresh_acquisition_pending());
        assert_eq!(
            track.acquisition_session_view(),
            Some(crate::runtime::AcquisitionSessionRuntimeView {
                acquisition_reset_reason: crate::runtime::AcquisitionResetReason::FreshSessionStart,
                acquisition_session_started_at: Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap(),
                initial_frontier: Some(Exposure(2.0)),
                initial_release_units: Some(0.0),
            })
        );

        manager.observe(&id, market(92.5)).unwrap();

        let track = manager.get_track(id.as_str()).unwrap();
        assert_eq!(
            track.inventory_decision().unwrap().execution_target,
            Exposure(2.0)
        );
        assert_eq!(
            track.acquisition_session_view().unwrap().initial_frontier,
            Some(Exposure(2.0))
        );
    }

    #[test]
    fn exchange_sync_installs_fresh_acquisition_initialization_after_resume() {
        let (mut manager, id) = manager();
        manager
            .fresh_start_track(
                &id,
                TrackState::Paused {
                    suspended: ControlState::Automatic(AutoState::FollowingBand),
                },
                TrackPnlStats::default(),
                FreshSessionExternalInputs {
                    current_exposure: Exposure(2.0),
                    position_qty: 2.0,
                    market_data: None,
                    instrument_spec: rules(),
                },
            )
            .unwrap();
        manager.observe(&id, market(92.5)).unwrap();
        manager.resume_track(id.as_str()).unwrap();

        manager
            .sync_exchange_state(
                &id,
                PositionObservation {
                    qty: 2.0,
                    unrealized_pnl: 0.0,
                },
                CompleteOpenOrderSnapshot::from_complete_exchange_query(vec![]),
                vec![],
            )
            .unwrap();

        let track = manager.get_track(id.as_str()).unwrap();
        assert!(!track.fresh_acquisition_pending());
        assert_eq!(
            track.acquisition_session_view().unwrap().initial_frontier,
            Some(Exposure(2.0))
        );
        assert_eq!(
            track.inventory_decision().unwrap().execution_target,
            Exposure(2.0)
        );
    }

    #[test]
    fn installing_planned_runtime_restores_pending_fresh_acquisition_session() {
        let (mut manager, id) = manager();
        manager
            .fresh_start_track(
                &id,
                TrackState::Running(ControlState::Automatic(AutoState::FollowingBand)),
                TrackPnlStats::default(),
                FreshSessionExternalInputs {
                    current_exposure: Exposure(2.0),
                    position_qty: 2.0,
                    market_data: None,
                    instrument_spec: rules(),
                },
            )
            .unwrap();
        let pending_state = manager.get_track(id.as_str()).cloned().unwrap();

        manager.observe(&id, market(92.5)).unwrap();
        assert!(
            !manager
                .get_track(id.as_str())
                .unwrap()
                .fresh_acquisition_pending()
        );

        manager.install_planned_runtime(pending_state);

        assert!(
            manager
                .get_track(id.as_str())
                .unwrap()
                .fresh_acquisition_pending()
        );
    }
}
