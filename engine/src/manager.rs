use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use poise_core::events::DomainEvent;
use poise_core::risk::{LossLimits, validate_loss_limits, validate_max_notional};
use poise_core::strategy::TrackConfig;
use poise_core::types::ExchangeRules;
use poise_core::types::Exposure;

use crate::command::TrackCommand;
use crate::execution_gate::ExecutionGateDecision;
use crate::executor;
use crate::ledger::{LedgerDelta, LedgerGapRecord};
use crate::observation::{
    MarketObservation, OrderObservation, PositionObservation, TrackObservation,
};
use crate::ports::{ClockPort, ExchangeOrder, ExecutionQuote, OrderReceipt, OrderRequest};
use crate::price_gate::{SubmitPurpose, evaluate_price_execution_gate};
use crate::reconciler;
use crate::runtime::{
    AutoState, ControlState, ExecutorState, ManualState, QuoteHealthView, StrategyPriceStatus,
    StrategyTargetView, TerminationCause, TrackLiveView, TrackRuntime, TrackState,
};
use crate::snapshot::TrackRuntimeSnapshot;
use crate::track::{Instrument, TrackId};
use crate::transition::{TrackEffect, TrackTransition};

const DEFAULT_TICK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeSyncMode {
    RecoverOnly,
    RecoverAndReconcile,
}

#[derive(Debug, Clone)]
pub enum MarketMutationOutcome {
    LiveOnly,
    Durable(TrackTransition),
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
        max_notional: f64,
        loss_limits: LossLimits,
        exchange_rules: ExchangeRules,
    ) -> Result<()> {
        self.add_track_with_tick_timeout_secs(
            id,
            instrument,
            config,
            max_notional,
            loss_limits,
            exchange_rules,
            DEFAULT_TICK_TIMEOUT_SECS,
        )
    }

    pub fn add_track_with_tick_timeout_secs(
        &mut self,
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
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
        validate_max_notional(max_notional).map_err(|e| anyhow::anyhow!(e))?;
        validate_loss_limits(&loss_limits).map_err(|e| anyhow::anyhow!(e))?;
        let track = TrackRuntime::new(
            id.clone(),
            instrument.clone(),
            config,
            max_notional,
            loss_limits,
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
        if let TrackObservation::Market(observation) = observation {
            return match self.observe_market_mutation(id, observation)? {
                MarketMutationOutcome::LiveOnly => self.transition_for(id, vec![], vec![]),
                MarketMutationOutcome::Durable(transition) => Ok(transition),
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
        let previous_snapshot = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .snapshot();
        let previous_active_risk_cap = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .active_risk_cap
            .clone();

        let (events, effects) = self.observe_market(id, observation)?;
        let next_snapshot = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?
            .snapshot();

        if market_mutation_requires_durable_write(
            &previous_snapshot,
            &next_snapshot,
            &events,
            &effects,
        ) {
            return Ok(MarketMutationOutcome::Durable(TrackTransition {
                snapshot: next_snapshot,
                events,
                effects,
            }));
        }

        let (
            strategy_price,
            strategy_price_status,
            mark_price,
            best_bid,
            best_ask,
            price_execution_gate,
            last_tick_at,
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
            )
        };

        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.restore_from_snapshot(&previous_snapshot)?;
        track.active_risk_cap = previous_active_risk_cap;
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
        let absorb_result = self.observe_order(id, observation)?;
        let (events, effects) = match (should_reconcile, self.live_strategy_price(id)?) {
            (true, Some(strategy_price)) => self.reconcile_track(id, strategy_price)?,
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
        track.executor_state = track.executor_state.reset_for_activation(activated_at);
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

        let resumed_at = self.clock.now();
        let resumed_state = {
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

            if let Some(strategy_price) = Self::live_strategy_price_for(track) {
                let mut resumed = track.clone();
                resumed.track_state = TrackState::WaitingMarketData;
                resumed.executor_state = track.executor_state.reset_for_activation(resumed_at);
                let result = self.plan_inventory_execution_for_track(&resumed, strategy_price)?;
                (
                    result.new_runtime_state.unwrap_or(TrackState::Running(
                        ControlState::Automatic(AutoState::FollowingBand),
                    )),
                    Some(result.desired_exposure.clone()),
                    result.execution_gate_decision,
                    executor::refresh_state(
                        &resumed.executor_state,
                        &resumed.config,
                        &resumed.current_exposure,
                        &result.desired_exposure,
                        resumed.config.min_rebalance_units,
                        resumed_at,
                    ),
                )
            } else {
                (
                    TrackState::WaitingMarketData,
                    None,
                    ExecutionGateDecision::Open,
                    track.executor_state.reset_for_activation(resumed_at),
                )
            }
        };

        let track = self
            .tracks
            .get_mut(&track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{id}` not found"))?;
        let (runtime_state, exposure, execution_gate_decision, executor_state) = resumed_state;
        track.track_state = runtime_state;
        track.execution_gate_state.last_decision = execution_gate_decision;
        Self::apply_targeting_state(track, exposure, None);
        track.executor_state = executor_state;

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
            Self::apply_targeting_state(track, Some(Exposure(0.0)), None);
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
            Self::apply_targeting_state(track, Some(Exposure(0.0)), None);
            Self::live_strategy_price_for(track)
        };

        match strategy_price {
            Some(strategy_price) => self.reconcile_track(id, strategy_price),
            None => Ok((vec![], vec![])),
        }
    }

    pub fn snapshot(&self, id: &str) -> Option<TrackRuntimeSnapshot> {
        self.get_track(id).map(TrackRuntime::snapshot)
    }

    pub fn track_live_view(&self, id: &TrackId) -> Result<TrackLiveView> {
        let track = self
            .tracks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        Ok(track.live_view())
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
        Ok(track.strategy_target_view())
    }

    pub fn restore_track_state(&mut self, snapshot: &TrackRuntimeSnapshot) -> Result<()> {
        let track = self
            .tracks
            .get_mut(&snapshot.track_id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", snapshot.track_id.as_str()))?;
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
                }
                Ok(())
            }
            executor::SubmitReceiptResolution::Unmatched => bail!(
                "submit receipt did not match executor binding: track=`{}`, client_order_id=`{}`, order_id=`{}`",
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
        }
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
        if next_state != track.executor_state {
            track.executor_state = next_state;
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
        }
        Ok(())
    }

    pub fn record_cancel_all_success(&mut self, id: &TrackId) -> Result<()> {
        self.clear_all_working_orders(id)
    }

    pub fn recover_submit_effect(
        &mut self,
        id: &TrackId,
        recovery_token: &executor::SubmitRecoveryToken,
        live_order: Option<&ExchangeOrder>,
    ) -> Result<executor::SubmitRecoveryPlan> {
        let live_order_observation = live_order.map(|order| OrderObservation {
            order_id: order.order_id.clone(),
            client_order_id: order.client_order_id.clone(),
            side: order.side,
            price: order.price,
            quantity: order.qty,
            filled_qty: order.filled_qty,
            realized_pnl: 0.0,
            status: order.status,
        });
        let plan = {
            let track = self
                .tracks
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
            executor::recover_submit_effect(executor::SubmitRecoveryInput {
                exchange_rules: &track.exchange_rules,
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
            if let Some(state) = plan.resolution.state()
                && state != &track.executor_state
            {
                track.executor_state = state.clone();
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
        let track = self
            .tracks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
        track.last_tick_at = Some(now);
        track.market_data_stale_since = None;
        track.mark_price = Some(observation.mark_price);
        track.best_bid = observation.execution_quote.map(|quote| quote.best_bid);
        track.best_ask = observation.execution_quote.map(|quote| quote.best_ask);
        track.price_execution_gate = evaluate_price_execution_gate(
            track.price_execution_gate,
            track.mark_price,
            observation.execution_quote,
        );

        let strategy_price = observation
            .execution_quote
            .map(|quote| (quote.best_bid + quote.best_ask) / 2.0);

        match strategy_price {
            Some(strategy_price) => {
                track.strategy_price = Some(strategy_price);
                track.strategy_price_status = StrategyPriceStatus::Live;
                self.reconcile_track(id, strategy_price)
            }
            None => {
                track.strategy_price_status = StrategyPriceStatus::Stale;
                Ok((vec![], vec![]))
            }
        }
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

        track.ledger_state.apply_delta(
            today,
            &crate::ledger::LedgerDelta::GrossRealizedPnl(observation.realized_pnl),
        );

        if track.executor_state.recovery_anomaly.is_some() {
            return Ok(executor::OrderUpdateAbsorbResult::DuplicateReplay);
        }

        let applied =
            executor::apply_order_observation_with_result(&track.executor_state, &observation);
        if applied.state != track.executor_state {
            track.executor_state = applied.state;
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
            config: &track.config,
            current_exposure: &track.current_exposure,
            base_qty_per_unit: track.config.base_qty_per_unit(),
            desired_exposure: track.desired_exposure.as_ref(),
            min_rebalance_units: track.config.min_rebalance_units,
            exchange_rules: &track.exchange_rules,
            previous_state: Some(&previous_state),
            live_orders: &open_orders,
            observed_at,
        });

        match recovery {
            executor::RecoveryResolution::Anomaly { state, .. } => {
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                track.executor_state = state;
                Ok((vec![], vec![TrackEffect::NoOp]))
            }
            executor::RecoveryResolution::Rebuilt { state } => {
                let mut planned_track = track.clone();
                planned_track.executor_state = state;

                if planned_track.track_state.is_paused() {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    return Ok((vec![], vec![]));
                }

                let Some(strategy_price) = Self::live_strategy_price_for(&planned_track) else {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    return Ok((vec![], vec![]));
                };

                if !mode.allows_follow_up_reconcile() {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    return Ok((vec![], vec![]));
                }

                if self.guard_market_data_freshness(id)? {
                    let track = self
                        .tracks
                        .get_mut(id)
                        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                    track.executor_state = planned_track.executor_state;
                    return Ok((vec![], vec![]));
                }

                let planned =
                    self.plan_inventory_execution_for_track(&planned_track, strategy_price)?;
                let effects = planned
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
                    .cloned()
                    .collect::<Vec<_>>();
                let track = self
                    .tracks
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;
                if let Some(new_runtime_state) = planned.new_runtime_state {
                    track.track_state = new_runtime_state;
                }
                track.execution_gate_state.last_decision = planned.execution_gate_decision;
                Self::apply_targeting_state(
                    track,
                    Some(planned.desired_exposure),
                    planned.applied_risk_cap,
                );
                track.executor_state = planned.executor_state;
                Ok((planned.events, effects))
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
        let PlannedInventoryExecution {
            events,
            effects: planned_effects,
            desired_exposure,
            applied_risk_cap,
            new_runtime_state,
            execution_gate_decision,
            executor_state,
        } = self.plan_inventory_execution_for_track(track, strategy_price)?;
        let effects = planned_effects;

        let track = self.tracks.get_mut(id).unwrap();
        if let Some(new_runtime_state) = new_runtime_state {
            track.track_state = new_runtime_state;
        }
        track.execution_gate_state.last_decision = execution_gate_decision;
        Self::apply_targeting_state(track, Some(desired_exposure), applied_risk_cap);
        track.executor_state = executor_state;

        Ok((events, effects))
    }

    fn clear_targeting_state(track: &mut TrackRuntime) {
        Self::apply_targeting_state(track, None, None);
    }

    fn apply_targeting_state(
        track: &mut TrackRuntime,
        desired_exposure: Option<Exposure>,
        active_risk_cap: Option<crate::runtime::AppliedRiskCap>,
    ) {
        track.desired_exposure = desired_exposure;
        track.active_risk_cap = active_risk_cap;
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
        desired_exposure: poise_core::types::Exposure,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> executor::SubmitIntentInput<'a> {
        let submit_purpose = self.submit_purpose_for_track(track, &desired_exposure);
        executor::SubmitIntentInput {
            instrument: &track.instrument,
            config: &track.config,
            exchange_rules: &track.exchange_rules,
            base_qty_per_unit: track.config.base_qty_per_unit(),
            min_rebalance_units: track.config.min_rebalance_units,
            current_exposure: track.current_exposure.clone(),
            desired_exposure,
            execution_quote: Self::execution_quote_for_track(track),
            policy_context: Self::policy_context_for_track(track),
            price_execution_gate: track.price_execution_gate,
            submit_purpose,
            observed_at,
        }
    }

    fn policy_context_for_track(track: &TrackRuntime) -> executor::PolicyContext {
        match &track.track_state {
            TrackState::Running(ControlState::Manual(_)) => executor::PolicyContext::ManualOverride,
            TrackState::Running(ControlState::Automatic(
                AutoState::Frozen { .. }
                | AutoState::FlattenPending { .. }
                | AutoState::Flattening { .. },
            ))
            | TrackState::Terminated { .. } => executor::PolicyContext::Flatten,
            _ => executor::PolicyContext::Normal,
        }
    }

    fn plan_inventory_execution_for_track(
        &self,
        track: &TrackRuntime,
        strategy_price: f64,
    ) -> Result<PlannedInventoryExecution> {
        let target = reconciler::reconcile_target(track, strategy_price);
        if track.executor_state.recovery_anomaly.is_some() {
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![TrackEffect::NoOp],
                desired_exposure: target.desired_exposure,
                applied_risk_cap: target.applied_risk_cap,
                new_runtime_state: target.new_runtime_state,
                execution_gate_decision: target.execution_gate_decision,
                executor_state: track.executor_state.clone(),
            });
        }
        let observed_at = self.clock.now();
        if target.suppress_execution {
            let executor_state = executor::refresh_state(
                &track.executor_state,
                &track.config,
                &track.current_exposure,
                &target.desired_exposure,
                track.config.min_rebalance_units,
                observed_at,
            );
            return Ok(PlannedInventoryExecution {
                events: target.events,
                effects: vec![TrackEffect::NoOp],
                desired_exposure: target.desired_exposure.clone(),
                applied_risk_cap: target.applied_risk_cap,
                new_runtime_state: target.new_runtime_state,
                execution_gate_decision: target.execution_gate_decision,
                executor_state,
            });
        }
        let executor_state = Some(&track.executor_state);
        let submit_intent =
            self.submit_intent_input(track, target.desired_exposure.clone(), observed_at);
        let plan = executor::plan(executor::ExecutorInput::new(submit_intent, executor_state));

        Ok(PlannedInventoryExecution {
            events: target.events,
            effects: plan.effects,
            desired_exposure: target.desired_exposure,
            applied_risk_cap: target.applied_risk_cap,
            new_runtime_state: target.new_runtime_state,
            execution_gate_decision: target.execution_gate_decision,
            executor_state: plan.state,
        })
    }
}

struct PlannedInventoryExecution {
    events: Vec<DomainEvent>,
    effects: Vec<TrackEffect>,
    desired_exposure: Exposure,
    applied_risk_cap: Option<crate::runtime::AppliedRiskCap>,
    new_runtime_state: Option<TrackState>,
    execution_gate_decision: ExecutionGateDecision,
    executor_state: ExecutorState,
}

fn market_mutation_requires_durable_write(
    previous_snapshot: &TrackRuntimeSnapshot,
    next_snapshot: &TrackRuntimeSnapshot,
    events: &[DomainEvent],
    effects: &[TrackEffect],
) -> bool {
    let desired_exposure_changed =
        previous_snapshot.desired_exposure != next_snapshot.desired_exposure;
    let has_non_target_events = events
        .iter()
        .any(|event| !matches!(event, DomainEvent::ExposureTargetChanged { .. }));
    let has_execution_effects = !effects.is_empty() && !matches!(effects, [TrackEffect::NoOp]);
    let snapshot_changed_without_target = {
        let mut normalized_next = next_snapshot.clone();
        normalized_next.desired_exposure = previous_snapshot.desired_exposure.clone();
        if !has_non_target_events && !has_execution_effects {
            normalized_next.executor_state = previous_snapshot.executor_state.clone();
        }
        normalized_next != *previous_snapshot
    };
    let target_reached_without_new_effects = next_snapshot
        .desired_exposure
        .as_ref()
        .is_some_and(|target| previous_snapshot.current_exposure.delta(target).is_zero());
    let durable_target_change = desired_exposure_changed
        && (has_execution_effects
            || has_non_target_events
            || target_reached_without_new_effects
            || next_snapshot.desired_exposure.is_none());

    snapshot_changed_without_target
        || has_non_target_events
        || has_execution_effects
        || durable_target_change
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::types::{ExchangeRules, Side};

    use crate::execution_plan::ExecutionAction;
    use crate::executor::{PolicyContext, policy::PolicyKind};
    use crate::ports::ExecutionQuote;
    use crate::track::Venue;

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
        manager
            .add_track(
                id.clone(),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                config(),
                10_000.0,
                loss_limits(),
                rules(),
            )
            .unwrap();
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
        }
    }

    fn loss_limits() -> LossLimits {
        LossLimits {
            daily_loss_limit: 1_000.0,
            total_loss_limit: 5_000.0,
        }
    }

    fn rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            quantity_step: 0.01,
            min_qty: 0.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0,
            taker_fee_rate: 0.0,
        }
    }

    fn market(price: f64) -> TrackObservation {
        TrackObservation::Market(MarketObservation {
            mark_price: price,
            execution_quote: Some(ExecutionQuote {
                best_bid: price - 0.1,
                best_ask: price + 0.1,
            }),
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

        let request = transition
            .effects
            .iter()
            .find_map(|effect| match effect {
                ExecutionAction::SubmitOrder { request, .. }
                    if request.client_order_id == catch_up_binding.request.client_order_id =>
                {
                    Some(request)
                }
                _ => None,
            })
            .expect("catch-up submit effect should exist");
        assert_eq!(request.side, Side::Buy);
        assert!((request.price - 95.1).abs() < 1e-9);
        assert!((request.quantity - 4.0).abs() < 1e-9);
        assert_eq!(catch_up_binding.allocations.len(), 4);
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
            track.config.notional_per_unit = 120.0;
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
    fn reconcile_track_projects_no_round_or_slot_state_after_executor_refresh() {
        let (mut manager, id) = manager();

        manager.observe(&id, market(100.0)).unwrap();

        let state_json = serde_json::to_value(&manager.snapshot(id.as_str()).unwrap()).unwrap();
        let executor_state = state_json.get("executor_state").unwrap();
        assert!(executor_state.get("active_round").is_none());
        assert!(executor_state.get("slots").is_none());
        assert!(executor_state.get("ledger_state").is_some());
        assert!(executor_state.get("bindings").is_some());
    }

    #[test]
    fn manager_maps_manual_track_state_to_manual_override_policy_context() {
        let (manager, id) = manager();
        let mut track = manager.tracks.get(&id).unwrap().clone();
        track.track_state = TrackState::Running(ControlState::Manual(ManualState::Flattened));

        assert_eq!(
            TrackManager::policy_context_for_track(&track),
            PolicyContext::ManualOverride
        );
    }

    #[test]
    fn manager_maps_protected_track_states_to_flatten_policy_context() {
        let (manager, id) = manager();
        let base_track = manager.tracks.get(&id).unwrap().clone();
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
            let mut track = base_track.clone();
            track.track_state = track_state;
            assert_eq!(
                TrackManager::policy_context_for_track(&track),
                PolicyContext::Flatten
            );
        }
    }
}
