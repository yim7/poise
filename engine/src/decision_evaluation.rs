//! Deterministic comparison between the curve-immediate benchmark and delayed engine decisions.

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use poise_core::instrument::{Amount, InstrumentSpec, PositionQty};
use poise_core::replay::{ReplayInput, ReplayReport, run_replay};
use poise_core::types::Side;

use crate::decision_replay::{
    DecisionReplayEffect, DecisionReplayFillModel, DecisionReplayInput,
    DecisionReplayReleaseTrigger, DecisionReplayReport, DecisionScenarioAction,
    run_decision_replay,
};
use crate::execution_gate::added_risk_units;
use crate::observation::MarketObservation;

/// Inputs that belong to evaluation rather than the pure decision runner.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEvaluationInput {
    pub replay: DecisionReplayInput,
    pub leverage: u32,
    pub ended_at: DateTime<Utc>,
}

/// Explicit live-time horizon used by every time-weighted metric.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEvaluationHorizon {
    pub first_live_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub wall_clock_duration_seconds: f64,
    pub live_duration_seconds: f64,
}

/// Quantity and weighted price for fills that add risk in one direction.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskIncreaseFillMetrics {
    pub position_qty: f64,
    pub weighted_fill_price: Option<f64>,
}

/// Execution and capital metrics shared by both evaluation lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionCostMetrics {
    pub long_risk_increase: RiskIncreaseFillMetrics,
    pub short_risk_increase: RiskIncreaseFillMetrics,
    pub turnover_notional: f64,
    pub estimated_taker_fee: f64,
    pub fee_asset: String,
    pub max_abs_notional: f64,
    pub max_margin_requirement: f64,
}

/// Metrics owned by the delayed decision lane.
#[derive(Debug, Clone, PartialEq)]
pub struct DelayedDecisionMetrics {
    pub execution: ExecutionCostMetrics,
    pub max_backlog_units: f64,
    pub time_weighted_mean_backlog_units: f64,
    pub max_authorized_but_unfilled_gap_units: f64,
    pub time_weighted_mean_authorized_but_unfilled_gap_units: f64,
    pub release_count: usize,
    pub max_single_catch_up_units: f64,
}

/// Matched-volume price comparison for one risk-increase direction.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRiskIncreasePriceComparison {
    pub curve_immediate_total_position_qty: f64,
    pub delayed_total_position_qty: f64,
    pub matched_position_qty: f64,
    pub curve_immediate_matched_weighted_price: Option<f64>,
    pub delayed_matched_weighted_price: Option<f64>,
    /// Positive means the delayed lane received a more favorable price.
    pub favorable_price_improvement: Option<f64>,
}

/// Tracking loss and matched-volume price improvement between the two lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEvaluationComparison {
    pub curve_immediate_time_weighted_tracking_error_units: f64,
    pub delayed_time_weighted_tracking_error_units: f64,
    pub additional_delayed_tracking_error_units: f64,
    pub curve_immediate_max_tracking_error_units: f64,
    pub delayed_max_tracking_error_units: f64,
    pub long_risk_increase_price: MatchedRiskIncreasePriceComparison,
    pub short_risk_increase_price: MatchedRiskIncreasePriceComparison,
}

/// Difference between a restarted path and the same actions with `Restart` removed.
#[derive(Debug, Clone, PartialEq)]
pub struct RestartSensitivityMetrics {
    pub time_weighted_mean_abs_execution_target_delta_units: f64,
    pub max_abs_execution_target_delta_units: f64,
    pub time_weighted_mean_abs_simulated_position_delta_units: f64,
    pub max_abs_simulated_position_delta_units: f64,
}

/// Side-by-side deterministic evaluation report.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEvaluationReport {
    pub fill_model: DecisionReplayFillModel,
    pub horizon: DecisionEvaluationHorizon,
    pub curve_immediate_benchmark: ReplayReport,
    pub delayed_decision_replay: DecisionReplayReport,
    pub curve_immediate_metrics: ExecutionCostMetrics,
    pub delayed_metrics: DelayedDecisionMetrics,
    pub comparison: DecisionEvaluationComparison,
    pub restart_sensitivity: Option<RestartSensitivityMetrics>,
}

/// Runs the real delayed decision path beside the curve-immediate benchmark.
///
/// Both lanes use deterministic fill assumptions. The report is an evaluation aid, not an
/// exchange-fill or PnL prediction.
pub fn run_decision_evaluation(
    input: &DecisionEvaluationInput,
) -> Result<DecisionEvaluationReport> {
    validate_evaluation_input(input)?;
    let delayed = run_decision_replay(&input.replay).context("run delayed decision replay")?;
    let curve_immediate = run_curve_immediate_benchmark(input, &delayed)?;
    let aligned = align_steps(input, &delayed, &curve_immediate)?;
    let live_window = build_live_window(input)?;

    let curve_fills = curve_fill_stream(input, &curve_immediate);
    let delayed_fills = delayed_fill_stream(&delayed);
    let curve_max_notional = max_lane_notional(
        &aligned,
        &live_window.active_after_step,
        &input.replay.instrument_spec,
        Lane::CurveImmediate,
        None,
    );
    let delayed_max_notional = max_lane_notional(
        &aligned,
        &live_window.active_after_step,
        &input.replay.instrument_spec,
        Lane::Delayed,
        Some(&delayed),
    );
    let curve_execution = execution_metrics(
        &curve_fills,
        curve_immediate.stats.estimated_fee,
        curve_immediate.stats.fee_asset.clone(),
        curve_max_notional,
        input.leverage,
    );
    let delayed_execution = execution_metrics(
        &delayed_fills,
        delayed.total_estimated_taker_fee,
        delayed.fee_asset.clone(),
        delayed_max_notional,
        input.leverage,
    );
    let tracking = tracking_metrics(&aligned, &live_window);
    let delayed_metrics = delayed_metrics(&aligned, &live_window, &delayed, delayed_execution);
    let comparison = DecisionEvaluationComparison {
        curve_immediate_time_weighted_tracking_error_units: tracking.curve_mean,
        delayed_time_weighted_tracking_error_units: tracking.delayed_mean,
        additional_delayed_tracking_error_units: tracking.delayed_mean - tracking.curve_mean,
        curve_immediate_max_tracking_error_units: tracking.curve_max,
        delayed_max_tracking_error_units: tracking.delayed_max,
        long_risk_increase_price: matched_price_comparison(
            &curve_fills.long,
            &delayed_fills.long,
            Side::Buy,
        ),
        short_risk_increase_price: matched_price_comparison(
            &curve_fills.short,
            &delayed_fills.short,
            Side::Sell,
        ),
    };
    let restart_sensitivity = restart_sensitivity(input, &delayed, &live_window)?;
    let first_live_at = input.replay.steps[live_window.intervals[0].step_index].observed_at;

    Ok(DecisionEvaluationReport {
        fill_model: input.replay.fill_model,
        horizon: DecisionEvaluationHorizon {
            first_live_at,
            ended_at: input.ended_at,
            wall_clock_duration_seconds: duration_seconds(input.replay.started_at, input.ended_at),
            live_duration_seconds: live_window.total_seconds,
        },
        curve_immediate_benchmark: curve_immediate,
        delayed_decision_replay: delayed,
        curve_immediate_metrics: curve_execution,
        delayed_metrics,
        comparison,
        restart_sensitivity,
    })
}

fn validate_evaluation_input(input: &DecisionEvaluationInput) -> Result<()> {
    ensure!(input.leverage > 0, "leverage must be positive");
    ensure!(
        input.ended_at >= input.replay.started_at,
        "ended_at must not be earlier than replay started_at"
    );
    for (index, step) in input.replay.steps.iter().enumerate() {
        ensure!(
            input.ended_at >= step.observed_at,
            "ended_at must not be earlier than step {index}"
        );
    }
    ensure!(
        input.replay.steps.iter().any(is_execution_quote_step),
        "decision evaluation requires at least one ExecutionQuote step"
    );

    if let Some(restart_index) = input
        .replay
        .steps
        .iter()
        .position(|step| matches!(&step.action, DecisionScenarioAction::Restart))
    {
        ensure!(
            !input.replay.steps[restart_index + 1..]
                .iter()
                .any(|step| matches!(&step.action, DecisionScenarioAction::Order(_))),
            "automatic restart sensitivity does not support Order(order_ref) after Restart"
        );
    }
    Ok(())
}

fn run_curve_immediate_benchmark(
    input: &DecisionEvaluationInput,
    delayed: &DecisionReplayReport,
) -> Result<ReplayReport> {
    let prices = input
        .replay
        .steps
        .iter()
        .zip(&delayed.steps)
        .filter_map(|(scenario, report)| {
            is_execution_quote_step(scenario).then_some(report.strategy_price)
        })
        .map(|price| price.context("ExecutionQuote step has no manager strategy_price"))
        .collect::<Result<Vec<_>>>()?;
    run_replay(&ReplayInput {
        prices,
        initial_position_qty: input.replay.initial_position.qty,
        leverage: input.leverage,
        track_config: input.replay.definition.track_config().clone(),
        instrument_spec: input.replay.instrument_spec.clone(),
        fee_rates: input.replay.fee_rates,
    })
    .map_err(anyhow::Error::msg)
    .context("run curve-immediate benchmark")
}

#[derive(Debug, Clone)]
struct AlignedStep {
    curve_target: Option<f64>,
    curve_position_before_qty: f64,
    curve_position_qty: f64,
    curve_position_exposure: f64,
    delayed_position_before_qty: f64,
    delayed_position_qty: f64,
    delayed_position_exposure: f64,
    backlog_units: f64,
    authorized_gap_units: f64,
    strategy_price: Option<f64>,
}

fn align_steps(
    input: &DecisionEvaluationInput,
    delayed: &DecisionReplayReport,
    curve: &ReplayReport,
) -> Result<Vec<AlignedStep>> {
    ensure!(
        input.replay.steps.len() == delayed.steps.len(),
        "delayed report step count does not match input"
    );
    let unit_qty = position_qty_per_exposure_unit(input)?;
    let mut curve_samples = curve.samples.iter();
    let mut curve_target = None;
    let mut curve_position_qty = input.replay.initial_position.qty;
    let mut delayed_position_qty = input.replay.initial_position.qty;
    let mut aligned = Vec::with_capacity(input.replay.steps.len());

    for (scenario, report) in input.replay.steps.iter().zip(&delayed.steps) {
        let curve_position_before_qty = curve_position_qty;
        if is_execution_quote_step(scenario) {
            let sample = curve_samples
                .next()
                .context("curve benchmark has fewer samples than ExecutionQuote steps")?;
            curve_target = Some(sample.target_exposure);
            curve_position_qty = sample.position_qty;
        }
        let delayed_position_before_qty = delayed_position_qty;
        delayed_position_qty = report.simulated_position_qty;
        let execution_target = report
            .committed_decision
            .as_ref()
            .map(|decision| decision.execution_target.0);
        let backlog_units = report
            .committed_decision
            .as_ref()
            .and_then(|decision| decision.risk_acquisition.as_ref())
            .map_or(0.0, |view| view.backlog_units);
        let authorized_gap_units = execution_target.map_or(0.0, |target| {
            (target - report.simulated_position_exposure.0).abs()
        });
        aligned.push(AlignedStep {
            curve_target,
            curve_position_before_qty,
            curve_position_qty,
            curve_position_exposure: curve_position_qty / unit_qty,
            delayed_position_before_qty,
            delayed_position_qty,
            delayed_position_exposure: report.simulated_position_exposure.0,
            backlog_units,
            authorized_gap_units,
            strategy_price: report.strategy_price,
        });
    }
    ensure!(
        curve_samples.next().is_none(),
        "curve benchmark has more samples than ExecutionQuote steps"
    );
    Ok(aligned)
}

#[derive(Debug, Clone, Copy)]
struct LiveInterval {
    step_index: usize,
    seconds: f64,
}

#[derive(Debug, Clone)]
struct LiveWindow {
    intervals: Vec<LiveInterval>,
    active_after_step: Vec<bool>,
    total_seconds: f64,
}

fn build_live_window(input: &DecisionEvaluationInput) -> Result<LiveWindow> {
    let mut live = false;
    let mut active_after_step = Vec::with_capacity(input.replay.steps.len());
    let mut intervals = Vec::new();

    for (index, step) in input.replay.steps.iter().enumerate() {
        match &step.action {
            DecisionScenarioAction::Restart => live = false,
            DecisionScenarioAction::Market(MarketObservation::ExecutionQuote { .. }) => {
                live = true;
            }
            DecisionScenarioAction::Market(MarketObservation::MarkPrice { .. })
            | DecisionScenarioAction::Position(_)
            | DecisionScenarioAction::Order(_) => {}
        }
        active_after_step.push(live);
        let next_at = input
            .replay
            .steps
            .get(index + 1)
            .map_or(input.ended_at, |next| next.observed_at);
        let next_is_restart = input
            .replay
            .steps
            .get(index + 1)
            .is_some_and(|next| matches!(&next.action, DecisionScenarioAction::Restart));
        let seconds = duration_seconds(step.observed_at, next_at);
        if live && !next_is_restart && seconds > 0.0 {
            intervals.push(LiveInterval {
                step_index: index,
                seconds,
            });
        }
    }

    let total_seconds = intervals.iter().map(|interval| interval.seconds).sum();
    ensure!(
        total_seconds > f64::EPSILON,
        "decision evaluation live duration must be positive"
    );
    Ok(LiveWindow {
        intervals,
        active_after_step,
        total_seconds,
    })
}

#[derive(Default)]
struct TrackingMetrics {
    curve_mean: f64,
    delayed_mean: f64,
    curve_max: f64,
    delayed_max: f64,
}

fn tracking_metrics(aligned: &[AlignedStep], window: &LiveWindow) -> TrackingMetrics {
    let mut metrics = TrackingMetrics::default();
    let mut curve_weighted = 0.0;
    let mut delayed_weighted = 0.0;
    for interval in &window.intervals {
        let step = &aligned[interval.step_index];
        let target = step
            .curve_target
            .expect("live interval must have a curve target");
        let curve_error = (target - step.curve_position_exposure).abs();
        let delayed_error = (target - step.delayed_position_exposure).abs();
        curve_weighted += curve_error * interval.seconds;
        delayed_weighted += delayed_error * interval.seconds;
    }
    for (step, active) in aligned.iter().zip(&window.active_after_step) {
        if !active {
            continue;
        }
        let target = step
            .curve_target
            .expect("active state must have a curve target");
        metrics.curve_max = metrics
            .curve_max
            .max((target - step.curve_position_exposure).abs());
        metrics.delayed_max = metrics
            .delayed_max
            .max((target - step.delayed_position_exposure).abs());
    }
    metrics.curve_mean = curve_weighted / window.total_seconds;
    metrics.delayed_mean = delayed_weighted / window.total_seconds;
    metrics
}

#[derive(Debug, Clone, Copy)]
struct FillSlice {
    price: f64,
    position_qty: f64,
}

#[derive(Default)]
struct FillStream {
    long: Vec<FillSlice>,
    short: Vec<FillSlice>,
    turnover_notional: f64,
}

fn curve_fill_stream(input: &DecisionEvaluationInput, report: &ReplayReport) -> FillStream {
    let mut stream = FillStream::default();
    let mut position_qty = input.replay.initial_position.qty;
    for sample in &report.samples {
        if sample.trade_position_qty.abs() <= f64::EPSILON {
            continue;
        }
        let next_position = position_qty + sample.trade_position_qty;
        let risk_increase = added_risk_units(position_qty, next_position);
        if risk_increase > f64::EPSILON {
            let fill = FillSlice {
                price: sample.price,
                position_qty: risk_increase,
            };
            if sample.trade_position_qty > 0.0 {
                stream.long.push(fill);
            } else {
                stream.short.push(fill);
            }
        }
        stream.turnover_notional += sample.trade_notional;
        position_qty = next_position;
    }
    stream
}

fn delayed_fill_stream(report: &DecisionReplayReport) -> FillStream {
    let mut stream = FillStream::default();
    for effect in report.steps.iter().flat_map(|step| &step.effects) {
        let DecisionReplayEffect::FillOrder {
            side,
            price,
            trade_notional,
            risk_increase_position_qty,
            ..
        } = effect
        else {
            continue;
        };
        stream.turnover_notional += trade_notional;
        if *risk_increase_position_qty <= f64::EPSILON {
            continue;
        }
        let fill = FillSlice {
            price: *price,
            position_qty: *risk_increase_position_qty,
        };
        match side {
            Side::Buy => stream.long.push(fill),
            Side::Sell => stream.short.push(fill),
        }
    }
    stream
}

fn execution_metrics(
    fills: &FillStream,
    estimated_taker_fee: f64,
    fee_asset: String,
    max_abs_notional: f64,
    leverage: u32,
) -> ExecutionCostMetrics {
    ExecutionCostMetrics {
        long_risk_increase: risk_fill_metrics(&fills.long),
        short_risk_increase: risk_fill_metrics(&fills.short),
        turnover_notional: fills.turnover_notional,
        estimated_taker_fee,
        fee_asset,
        max_abs_notional,
        max_margin_requirement: max_abs_notional / f64::from(leverage),
    }
}

fn risk_fill_metrics(fills: &[FillSlice]) -> RiskIncreaseFillMetrics {
    let position_qty = fills.iter().map(|fill| fill.position_qty).sum::<f64>();
    let weighted_fill_price = (position_qty > f64::EPSILON).then(|| {
        fills
            .iter()
            .map(|fill| fill.price * fill.position_qty)
            .sum::<f64>()
            / position_qty
    });
    RiskIncreaseFillMetrics {
        position_qty,
        weighted_fill_price,
    }
}

fn delayed_metrics(
    aligned: &[AlignedStep],
    window: &LiveWindow,
    delayed: &DecisionReplayReport,
    execution: ExecutionCostMetrics,
) -> DelayedDecisionMetrics {
    let mut max_backlog: f64 = 0.0;
    let mut backlog_weighted = 0.0;
    let mut max_gap: f64 = 0.0;
    let mut gap_weighted = 0.0;
    for (step, active) in aligned.iter().zip(&window.active_after_step) {
        if *active {
            max_backlog = max_backlog.max(step.backlog_units);
            max_gap = max_gap.max(step.authorized_gap_units);
        }
    }
    for interval in &window.intervals {
        let step = &aligned[interval.step_index];
        backlog_weighted += step.backlog_units * interval.seconds;
        gap_weighted += step.authorized_gap_units * interval.seconds;
    }
    let mut release_count = 0usize;
    let mut max_single_catch_up: f64 = 0.0;
    for effect in delayed.steps.iter().flat_map(|step| &step.effects) {
        let DecisionReplayEffect::RiskReleaseApplied {
            trigger,
            release_units,
            ..
        } = effect
        else {
            continue;
        };
        release_count += 1;
        if matches!(
            trigger,
            DecisionReplayReleaseTrigger::PriceAdvantage
                | DecisionReplayReleaseTrigger::StaleTimeout
        ) {
            max_single_catch_up = max_single_catch_up.max(*release_units);
        }
    }
    DelayedDecisionMetrics {
        execution,
        max_backlog_units: max_backlog,
        time_weighted_mean_backlog_units: backlog_weighted / window.total_seconds,
        max_authorized_but_unfilled_gap_units: max_gap,
        time_weighted_mean_authorized_but_unfilled_gap_units: gap_weighted / window.total_seconds,
        release_count,
        max_single_catch_up_units: max_single_catch_up,
    }
}

fn matched_price_comparison(
    curve: &[FillSlice],
    delayed: &[FillSlice],
    direction: Side,
) -> MatchedRiskIncreasePriceComparison {
    let curve_total = curve.iter().map(|fill| fill.position_qty).sum::<f64>();
    let delayed_total = delayed.iter().map(|fill| fill.position_qty).sum::<f64>();
    let matched = curve_total.min(delayed_total);
    let curve_price = prefix_weighted_price(curve, matched);
    let delayed_price = prefix_weighted_price(delayed, matched);
    let improvement = match (curve_price, delayed_price, direction) {
        (Some(curve), Some(delayed), Side::Buy) => Some(curve - delayed),
        (Some(curve), Some(delayed), Side::Sell) => Some(delayed - curve),
        _ => None,
    };
    MatchedRiskIncreasePriceComparison {
        curve_immediate_total_position_qty: curve_total,
        delayed_total_position_qty: delayed_total,
        matched_position_qty: matched,
        curve_immediate_matched_weighted_price: curve_price,
        delayed_matched_weighted_price: delayed_price,
        favorable_price_improvement: improvement,
    }
}

fn prefix_weighted_price(fills: &[FillSlice], matched_qty: f64) -> Option<f64> {
    if matched_qty <= f64::EPSILON {
        return None;
    }
    let mut remaining = matched_qty;
    let mut weighted = 0.0;
    for fill in fills {
        let used = remaining.min(fill.position_qty);
        weighted += used * fill.price;
        remaining -= used;
        if remaining <= f64::EPSILON {
            break;
        }
    }
    Some(weighted / matched_qty)
}

#[derive(Clone, Copy)]
enum Lane {
    CurveImmediate,
    Delayed,
}

fn max_lane_notional(
    aligned: &[AlignedStep],
    active: &[bool],
    instrument_spec: &InstrumentSpec,
    lane: Lane,
    delayed: Option<&DecisionReplayReport>,
) -> f64 {
    let mut max_notional: f64 = 0.0;
    for (index, (step, active)) in aligned.iter().zip(active).enumerate() {
        if !active {
            continue;
        }
        let Some(price) = step.strategy_price else {
            continue;
        };
        let (before, after) = match lane {
            Lane::CurveImmediate => (step.curve_position_before_qty, step.curve_position_qty),
            Lane::Delayed => (step.delayed_position_before_qty, step.delayed_position_qty),
        };
        max_notional = max_notional
            .max(position_notional(instrument_spec, before, price))
            .max(position_notional(instrument_spec, after, price));
        if let (Lane::Delayed, Some(report)) = (lane, delayed) {
            for effect in &report.steps[index].effects {
                if let DecisionReplayEffect::FillOrder {
                    position_before_qty,
                    position_after_qty,
                    ..
                } = effect
                {
                    max_notional = max_notional
                        .max(position_notional(
                            instrument_spec,
                            *position_before_qty,
                            price,
                        ))
                        .max(position_notional(
                            instrument_spec,
                            *position_after_qty,
                            price,
                        ));
                }
            }
        }
    }
    max_notional
}

fn restart_sensitivity(
    input: &DecisionEvaluationInput,
    restarted: &DecisionReplayReport,
    window: &LiveWindow,
) -> Result<Option<RestartSensitivityMetrics>> {
    if !input
        .replay
        .steps
        .iter()
        .any(|step| matches!(&step.action, DecisionScenarioAction::Restart))
    {
        return Ok(None);
    }
    let mut continuous_input = input.replay.clone();
    continuous_input
        .steps
        .retain(|step| !matches!(&step.action, DecisionScenarioAction::Restart));
    let continuous =
        run_decision_replay(&continuous_input).context("run no-restart decision counterfactual")?;
    let mut continuous_index_by_original = vec![None; input.replay.steps.len()];
    let mut continuous_index = 0usize;
    for (index, step) in input.replay.steps.iter().enumerate() {
        if matches!(&step.action, DecisionScenarioAction::Restart) {
            continue;
        }
        continuous_index_by_original[index] = Some(continuous_index);
        continuous_index += 1;
    }

    let mut execution_weighted = 0.0;
    let mut position_weighted = 0.0;
    let mut max_execution: f64 = 0.0;
    let mut max_position: f64 = 0.0;
    let deltas_at = |index: usize| {
        let restarted_step = &restarted.steps[index];
        let continuous_step = &continuous.steps
            [continuous_index_by_original[index].expect("active state cannot be Restart")];
        let restarted_execution = restarted_step
            .committed_decision
            .as_ref()
            .map_or(restarted_step.simulated_position_exposure.0, |decision| {
                decision.execution_target.0
            });
        let continuous_execution = continuous_step
            .committed_decision
            .as_ref()
            .map_or(continuous_step.simulated_position_exposure.0, |decision| {
                decision.execution_target.0
            });
        (
            (restarted_execution - continuous_execution).abs(),
            (restarted_step.simulated_position_exposure.0
                - continuous_step.simulated_position_exposure.0)
                .abs(),
        )
    };
    for interval in &window.intervals {
        let (execution_delta, position_delta) = deltas_at(interval.step_index);
        execution_weighted += execution_delta * interval.seconds;
        position_weighted += position_delta * interval.seconds;
    }
    for (index, active) in window.active_after_step.iter().enumerate() {
        if *active {
            let (execution_delta, position_delta) = deltas_at(index);
            max_execution = max_execution.max(execution_delta);
            max_position = max_position.max(position_delta);
        }
    }
    Ok(Some(RestartSensitivityMetrics {
        time_weighted_mean_abs_execution_target_delta_units: execution_weighted
            / window.total_seconds,
        max_abs_execution_target_delta_units: max_execution,
        time_weighted_mean_abs_simulated_position_delta_units: position_weighted
            / window.total_seconds,
        max_abs_simulated_position_delta_units: max_position,
    }))
}

fn position_qty_per_exposure_unit(input: &DecisionEvaluationInput) -> Result<f64> {
    let config = input.replay.definition.track_config();
    let unit_qty = input
        .replay
        .instrument_spec
        .position_qty_for_notional(
            Amount::new(
                config.notional_per_unit,
                input.replay.instrument_spec.notional_asset().clone(),
            )
            .map_err(anyhow::Error::msg)?,
            config.band_center(),
        )
        .map_err(anyhow::Error::msg)?
        .value();
    ensure!(
        unit_qty.is_finite() && unit_qty > f64::EPSILON,
        "position quantity per exposure unit must be finite and positive"
    );
    Ok(unit_qty)
}

fn position_notional(instrument_spec: &InstrumentSpec, quantity: f64, price: f64) -> f64 {
    instrument_spec
        .notional_from_position_qty(
            PositionQty::new(quantity).expect("evaluated position quantity must be finite"),
            price,
        )
        .expect("validated evaluation input must define position notional")
        .value()
}

fn is_execution_quote_step(step: &crate::decision_replay::DecisionScenarioStep) -> bool {
    matches!(
        &step.action,
        DecisionScenarioAction::Market(MarketObservation::ExecutionQuote { .. })
    )
}

fn duration_seconds(start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    end.signed_duration_since(start)
        .to_std()
        .map_or(0.0, |duration| duration.as_secs_f64())
}
