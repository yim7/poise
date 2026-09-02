use chrono::{DateTime, Duration, TimeZone, Utc};
use poise_core::{
    instrument::{Amount, Asset, InstrumentSpec, PositionBasis},
    replay::ReplayFeeRates,
    risk::LossLimits,
    strategy::{ShapeFamily, TrackConfig, desired_exposure},
    track::{Instrument, TrackDefinition, TrackId, Venue},
    types::PricePrecision,
};
use poise_engine::{
    decision_evaluation::{
        DecisionEvaluationInput, DecisionEvaluationReport, ExecutionCostMetrics,
        MatchedRiskIncreasePriceComparison, run_decision_evaluation,
    },
    decision_replay::{
        DecisionReplayEffect, DecisionReplayFillModel, DecisionReplayInput,
        DecisionReplayReleaseTrigger, DecisionScenarioAction, DecisionScenarioStep,
    },
    execution_plan::DecisionReason,
    observation::{MarketObservation, PositionObservation},
    ports::ExecutionQuote,
    runtime::RiskAcquisitionDirection,
};
use serde::{Deserialize, Serialize};

use crate::{
    commands::TrackDraftPayload,
    config_document::{TrackDraft, TrackShapeFamily},
    error::{CommandError, CommandErrorKind},
};

const CURVE_POINT_COUNT: usize = 41;
const INSTRUMENT_FIXTURE_NOTICE: &str = "确定性评估假设，不是交易所实时规则";
const FILL_MODEL_NOTICE: &str =
    "ImmediateFullAtOrderPrice 只是确定性成交假设，不是成交概率、滑点或 PnL 预测";

#[derive(Debug, Clone)]
struct EvaluationInstrumentFixture {
    instrument_spec: InstrumentSpec,
    fee_rates: ReplayFeeRates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationInstrumentFixtureId {
    BaseAssetV1,
    InverseContractV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkbenchEvaluationRequest {
    pub draft: TrackDraftPayload,
    pub exchange_venue: String,
    pub instrument_fixture: EvaluationInstrumentFixtureId,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEvaluationReportDto {
    pub instrument_fixture: EvaluationInstrumentFixtureDto,
    pub assumptions: EvaluationAssumptionsDto,
    pub curve_points: Vec<CurvePointDto>,
    pub scenarios: Vec<ScenarioReportDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationInstrumentFixtureDto {
    pub fixture_id: EvaluationInstrumentFixtureId,
    pub base_asset: String,
    pub quote_asset: String,
    pub notional_asset: String,
    pub price_tick: f64,
    pub price_precision: EvaluationPricePrecisionDto,
    pub position_basis: EvaluationPositionBasisDto,
    pub contract_value: Option<f64>,
    pub settlement_asset: String,
    pub position_qty_step: f64,
    pub min_position_qty: f64,
    pub min_notional: f64,
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
    pub position_qty_per_exposure_unit: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvaluationPricePrecisionDto {
    FixedTick,
    SignificantFigures {
        #[serde(rename = "maxDecimals")]
        max_decimals: u32,
        #[serde(rename = "significantFigures")]
        significant_figures: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationPositionBasisDto {
    BaseAsset,
    Contracts,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationAssumptionsDto {
    pub instrument_fixture_notice: String,
    pub fill_model_notice: String,
    pub initial_position_qty: f64,
    pub initial_exposure: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurvePointDto {
    pub price: f64,
    pub target_exposure: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationDirectionDto {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationFillModelDto {
    ImmediateFullAtOrderPrice,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioReportDto {
    pub id: String,
    pub title: String,
    pub direction: EvaluationDirectionDto,
    pub fill_model: EvaluationFillModelDto,
    pub horizon: EvaluationHorizonDto,
    pub assumptions: ScenarioAssumptionsDto,
    pub curve_immediate_benchmark: EvaluationLaneDto,
    pub delayed_decision_replay: DelayedEvaluationLaneDto,
    pub comparison: EvaluationComparisonDto,
    pub replay_rows: Vec<ReplayRowDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationHorizonDto {
    pub first_live_at: String,
    pub ended_at: String,
    pub wall_clock_duration_seconds: f64,
    pub live_duration_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioAssumptionsDto {
    pub initial_position_qty: f64,
    pub initial_exposure: f64,
    pub deterministic_fill_notice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationLaneDto {
    pub execution: ExecutionMetricsDto,
    pub tracking: TrackingMetricsDto,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelayedEvaluationLaneDto {
    pub execution: ExecutionMetricsDto,
    pub tracking: TrackingMetricsDto,
    pub backlog: DelayedBacklogMetricsDto,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionMetricsDto {
    pub long_risk_increase: RiskIncreaseFillMetricsDto,
    pub short_risk_increase: RiskIncreaseFillMetricsDto,
    pub turnover_notional: f64,
    pub estimated_taker_fee: f64,
    pub fee_asset: String,
    pub max_abs_notional: f64,
    pub max_margin_requirement: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskIncreaseFillMetricsDto {
    pub position_qty: f64,
    pub weighted_fill_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackingMetricsDto {
    pub time_weighted_tracking_error_units: f64,
    pub max_tracking_error_units: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelayedBacklogMetricsDto {
    pub max_backlog_units: f64,
    pub time_weighted_mean_backlog_units: f64,
    pub max_authorized_but_unfilled_gap_units: f64,
    pub time_weighted_mean_authorized_but_unfilled_gap_units: f64,
    pub release_count: usize,
    pub max_single_catch_up_units: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationComparisonDto {
    pub curve_immediate_time_weighted_tracking_error_units: f64,
    pub delayed_time_weighted_tracking_error_units: f64,
    pub additional_delayed_tracking_error_units: f64,
    pub curve_immediate_max_tracking_error_units: f64,
    pub delayed_max_tracking_error_units: f64,
    pub long_risk_increase_price: MatchedPriceComparisonDto,
    pub short_risk_increase_price: MatchedPriceComparisonDto,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchedPriceComparisonDto {
    pub curve_immediate_total_position_qty: f64,
    pub delayed_total_position_qty: f64,
    pub matched_position_qty: f64,
    pub curve_immediate_matched_weighted_price: Option<f64>,
    pub delayed_matched_weighted_price: Option<f64>,
    pub favorable_price_improvement: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRowDto {
    pub observed_at: String,
    pub action: String,
    pub strategy_price: Option<f64>,
    pub simulated_position_qty: f64,
    pub simulated_position_exposure: f64,
    pub observed_position_qty: f64,
    pub observed_position_exposure: f64,
    pub signed_filled_position_qty: f64,
    pub policy_target: Option<f64>,
    pub risk_approved_target: Option<f64>,
    pub acquisition_frontier: Option<f64>,
    pub execution_target: Option<f64>,
    pub reason: Option<String>,
    pub revision: Option<u64>,
    pub applied_releases: Vec<AppliedReleaseDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationReleaseTriggerDto {
    InitialRatio,
    PriceAdvantage,
    StaleTimeout,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedReleaseDto {
    pub trigger: EvaluationReleaseTriggerDto,
    pub direction: EvaluationDirectionDto,
    pub frontier_before: f64,
    pub frontier_after: f64,
    pub release_units: f64,
}

pub fn evaluate_workbench_draft(
    request: WorkbenchEvaluationRequest,
) -> Result<WorkbenchEvaluationReportDto, CommandError> {
    let fixture_id = request.instrument_fixture;
    let leverage = request.draft.fields.leverage;
    let draft = TrackDraft::try_from(request.draft)
        .map_err(|error| evaluation_error(format!("draft 配置无效: {error}")))?;
    let definition = build_definition(draft, &request.exchange_venue)?;
    let fixture = instrument_fixture(fixture_id, definition.instrument().clone());
    let track_config = definition.track_config();
    let unit_qty = fixture
        .instrument_spec
        .position_qty_for_notional(
            Amount::new(
                track_config.notional_per_unit,
                fixture.instrument_spec.notional_asset().clone(),
            )
            .map_err(evaluation_error)?,
            track_config.band_center(),
        )
        .map_err(evaluation_error)?
        .value();
    if !unit_qty.is_finite() || unit_qty <= f64::EPSILON {
        return Err(evaluation_error(
            "instrument fixture 无法为当前配置生成有效的每单位仓位数量",
        ));
    }

    let mut scenarios = Vec::new();
    if track_config.long_exposure_units > 0.0 {
        scenarios.push(run_scenario(
            &definition,
            &fixture,
            leverage,
            ScenarioKind::PriceAdvantageSweep,
            EvaluationDirectionDto::Long,
        )?);
        scenarios.push(run_scenario(
            &definition,
            &fixture,
            leverage,
            ScenarioKind::StaleHold,
            EvaluationDirectionDto::Long,
        )?);
    }
    if track_config.short_exposure_units > 0.0 {
        scenarios.push(run_scenario(
            &definition,
            &fixture,
            leverage,
            ScenarioKind::PriceAdvantageSweep,
            EvaluationDirectionDto::Short,
        )?);
        scenarios.push(run_scenario(
            &definition,
            &fixture,
            leverage,
            ScenarioKind::StaleHold,
            EvaluationDirectionDto::Short,
        )?);
    }

    Ok(WorkbenchEvaluationReportDto {
        instrument_fixture: project_instrument_fixture(fixture_id, &fixture, unit_qty),
        assumptions: EvaluationAssumptionsDto {
            instrument_fixture_notice: INSTRUMENT_FIXTURE_NOTICE.to_string(),
            fill_model_notice: FILL_MODEL_NOTICE.to_string(),
            initial_position_qty: 0.0,
            initial_exposure: 0.0,
        },
        curve_points: curve_points(track_config),
        scenarios,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScenarioKind {
    PriceAdvantageSweep,
    StaleHold,
}

fn build_definition(
    draft: TrackDraft,
    exchange_venue: &str,
) -> Result<TrackDefinition, CommandError> {
    let fields = draft.fields;
    let venue = parse_venue(exchange_venue)?;
    let shape_family = match fields.shape_family {
        TrackShapeFamily::Linear => ShapeFamily::Linear,
        TrackShapeFamily::Inertial => ShapeFamily::Inertial,
        TrackShapeFamily::Responsive => ShapeFamily::Responsive,
    };
    TrackDefinition::try_new(
        TrackId::new(fields.track_id),
        Instrument::new(venue, fields.symbol),
        TrackConfig {
            lower_price: fields.lower_price,
            upper_price: fields.upper_price,
            long_exposure_units: fields.long_exposure_units,
            short_exposure_units: fields.short_exposure_units,
            notional_per_unit: fields.notional_per_unit,
            min_rebalance_units: fields.min_rebalance_units,
            shape_family,
            out_of_band_policy: fields.out_of_band_policy,
            risk_acquisition: fields.risk_acquisition,
        },
        Some(fields.max_notional),
        LossLimits {
            daily_loss_limit: fields.daily_loss_limit,
            total_loss_limit: fields.total_loss_limit,
        },
        None,
    )
    .map_err(|error| evaluation_error(format!("track 配置无效: {error}")))
}

fn parse_venue(value: &str) -> Result<Venue, CommandError> {
    match value.trim() {
        "binance" => Ok(Venue::Binance),
        "bybit" => Ok(Venue::Bybit),
        "hyperliquid" => Ok(Venue::Hyperliquid),
        "okx" => Ok(Venue::Okx),
        other => Err(evaluation_error(format!(
            "不支持的 exchange_venue: `{other}`"
        ))),
    }
}

fn instrument_fixture(
    fixture_id: EvaluationInstrumentFixtureId,
    instrument: Instrument,
) -> EvaluationInstrumentFixture {
    let fee_rates = ReplayFeeRates {
        maker_fee_rate: 0.0002,
        taker_fee_rate: 0.001,
    };
    match fixture_id {
        EvaluationInstrumentFixtureId::BaseAssetV1 => {
            let usdt = Asset::try_new("USDT").expect("fixed fixture asset must be valid");
            EvaluationInstrumentFixture {
                instrument_spec: InstrumentSpec::try_linear(
                    instrument,
                    Asset::try_new("BTC").expect("fixed fixture asset must be valid"),
                    usdt.clone(),
                    usdt.clone(),
                    PricePrecision::FixedTick,
                    0.00000001,
                    0.00000001,
                    0.0,
                    Amount::new(0.0, usdt).expect("fixed fixture amount must be valid"),
                )
                .expect("fixed linear fixture must be valid"),
                fee_rates,
            }
        }
        EvaluationInstrumentFixtureId::InverseContractV1 => {
            let usd = Asset::try_new("USD").expect("fixed fixture asset must be valid");
            EvaluationInstrumentFixture {
                instrument_spec: InstrumentSpec::try_inverse(
                    instrument,
                    Asset::try_new("BTC").expect("fixed fixture asset must be valid"),
                    usd.clone(),
                    Asset::try_new("BTC").expect("fixed fixture asset must be valid"),
                    Amount::new(100.0, usd.clone()).expect("fixed fixture amount must be valid"),
                    PricePrecision::FixedTick,
                    0.1,
                    1.0,
                    1.0,
                    Amount::new(100.0, usd).expect("fixed fixture amount must be valid"),
                )
                .expect("fixed inverse fixture must be valid"),
                fee_rates,
            }
        }
    }
}

fn curve_points(config: &TrackConfig) -> Vec<CurvePointDto> {
    let price_span = config.upper_price - config.lower_price;
    (0..CURVE_POINT_COUNT)
        .map(|index| {
            let progress = index as f64 / (CURVE_POINT_COUNT - 1) as f64;
            let price = config.lower_price + price_span * progress;
            CurvePointDto {
                price,
                target_exposure: desired_exposure(price, config).0,
            }
        })
        .collect()
}

fn run_scenario(
    definition: &TrackDefinition,
    fixture: &EvaluationInstrumentFixture,
    leverage: u32,
    kind: ScenarioKind,
    direction: EvaluationDirectionDto,
) -> Result<ScenarioReportDto, CommandError> {
    let input = build_scenario_input(definition, fixture, leverage, kind, direction)?;
    let report = run_decision_evaluation(&input).map_err(|error| {
        evaluation_error(format!(
            "Rust decision evaluator 执行失败（{}）: {error:#}",
            scenario_id(kind, direction)
        ))
    })?;
    Ok(project_scenario(kind, direction, &report))
}

fn build_scenario_input(
    definition: &TrackDefinition,
    fixture: &EvaluationInstrumentFixture,
    leverage: u32,
    kind: ScenarioKind,
    direction: EvaluationDirectionDto,
) -> Result<DecisionEvaluationInput, CommandError> {
    let config = definition.track_config();
    let started_at = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("fixed evaluation timestamp must be valid");
    let zero_target_price = zero_target_price(config);
    let half_price = match direction {
        EvaluationDirectionDto::Long => (zero_target_price + config.lower_price) / 2.0,
        EvaluationDirectionDto::Short => (zero_target_price + config.upper_price) / 2.0,
    };
    let boundary_price = match direction {
        EvaluationDirectionDto::Long => config.lower_price,
        EvaluationDirectionDto::Short => config.upper_price,
    };
    let backlog_at = checked_add(started_at, Duration::minutes(1))?;
    let mut steps = vec![
        quote_step(started_at, zero_target_price),
        quote_step(backlog_at, half_price),
    ];
    let ended_at = match kind {
        ScenarioKind::PriceAdvantageSweep => {
            steps.push(quote_step(
                checked_add(started_at, Duration::minutes(2))?,
                boundary_price,
            ));
            checked_add(started_at, Duration::minutes(3))?
        }
        ScenarioKind::StaleHold => {
            let threshold_milliseconds =
                stale_threshold_milliseconds(config.risk_acquisition.stale_release_minutes)?;
            if threshold_milliseconds > 0 {
                let before_milliseconds = threshold_milliseconds.saturating_sub(1_000);
                steps.push(quote_step(
                    checked_add(backlog_at, duration_milliseconds(before_milliseconds)?)?,
                    half_price,
                ));
                steps.push(quote_step(
                    checked_add(backlog_at, duration_milliseconds(threshold_milliseconds)?)?,
                    half_price,
                ));
            }
            let ended_after_milliseconds = threshold_milliseconds
                .checked_add(60_000)
                .ok_or_else(|| evaluation_error("评估场景时间超出支持范围"))?;
            checked_add(backlog_at, duration_milliseconds(ended_after_milliseconds)?)?
        }
    };
    Ok(DecisionEvaluationInput {
        replay: DecisionReplayInput {
            definition: definition.clone(),
            instrument_spec: fixture.instrument_spec.clone(),
            fee_rates: fixture.fee_rates,
            started_at,
            initial_position: PositionObservation {
                qty: 0.0,
                unrealized_pnl: 0.0,
            },
            steps,
            fill_model: DecisionReplayFillModel::ImmediateFullAtOrderPrice,
        },
        leverage,
        ended_at,
    })
}

fn zero_target_price(config: &TrackConfig) -> f64 {
    let lower_target = desired_exposure(config.lower_price, config).0;
    if lower_target == 0.0 {
        return config.lower_price;
    }
    let upper_target = desired_exposure(config.upper_price, config).0;
    if upper_target == 0.0 {
        return config.upper_price;
    }

    let mut lower = config.lower_price;
    let mut upper = config.upper_price;
    for _ in 0..100 {
        let middle = (lower + upper) / 2.0;
        if desired_exposure(middle, config).0 > 0.0 {
            lower = middle;
        } else {
            upper = middle;
        }
    }
    (lower + upper) / 2.0
}

fn stale_threshold_milliseconds(minutes: f64) -> Result<i64, CommandError> {
    let milliseconds = (minutes * 60_000.0).ceil();
    if !milliseconds.is_finite() || milliseconds < 0.0 || milliseconds > i64::MAX as f64 {
        return Err(evaluation_error(
            "stale_release_minutes 超出可生成场景的时间范围",
        ));
    }
    Ok(milliseconds as i64)
}

fn checked_add(
    timestamp: DateTime<Utc>,
    duration: Duration,
) -> Result<DateTime<Utc>, CommandError> {
    timestamp
        .checked_add_signed(duration)
        .ok_or_else(|| evaluation_error("评估场景时间超出支持范围"))
}

fn duration_milliseconds(milliseconds: i64) -> Result<Duration, CommandError> {
    Duration::try_milliseconds(milliseconds)
        .ok_or_else(|| evaluation_error("评估场景时间超出支持范围"))
}

fn quote_step(observed_at: DateTime<Utc>, price: f64) -> DecisionScenarioStep {
    DecisionScenarioStep {
        observed_at,
        action: DecisionScenarioAction::Market(MarketObservation::ExecutionQuote {
            execution_quote: ExecutionQuote {
                best_bid: price,
                best_ask: price,
            },
        }),
    }
}

fn project_scenario(
    kind: ScenarioKind,
    direction: EvaluationDirectionDto,
    report: &DecisionEvaluationReport,
) -> ScenarioReportDto {
    let comparison = &report.comparison;
    ScenarioReportDto {
        id: scenario_id(kind, direction),
        title: scenario_title(kind, direction),
        direction,
        fill_model: EvaluationFillModelDto::ImmediateFullAtOrderPrice,
        horizon: EvaluationHorizonDto {
            first_live_at: report.horizon.first_live_at.to_rfc3339(),
            ended_at: report.horizon.ended_at.to_rfc3339(),
            wall_clock_duration_seconds: report.horizon.wall_clock_duration_seconds,
            live_duration_seconds: report.horizon.live_duration_seconds,
        },
        assumptions: ScenarioAssumptionsDto {
            initial_position_qty: 0.0,
            initial_exposure: 0.0,
            deterministic_fill_notice: FILL_MODEL_NOTICE.to_string(),
        },
        curve_immediate_benchmark: EvaluationLaneDto {
            execution: project_execution(&report.curve_immediate_metrics),
            tracking: TrackingMetricsDto {
                time_weighted_tracking_error_units: comparison
                    .curve_immediate_time_weighted_tracking_error_units,
                max_tracking_error_units: comparison.curve_immediate_max_tracking_error_units,
            },
        },
        delayed_decision_replay: DelayedEvaluationLaneDto {
            execution: project_execution(&report.delayed_metrics.execution),
            tracking: TrackingMetricsDto {
                time_weighted_tracking_error_units: comparison
                    .delayed_time_weighted_tracking_error_units,
                max_tracking_error_units: comparison.delayed_max_tracking_error_units,
            },
            backlog: DelayedBacklogMetricsDto {
                max_backlog_units: report.delayed_metrics.max_backlog_units,
                time_weighted_mean_backlog_units: report
                    .delayed_metrics
                    .time_weighted_mean_backlog_units,
                max_authorized_but_unfilled_gap_units: report
                    .delayed_metrics
                    .max_authorized_but_unfilled_gap_units,
                time_weighted_mean_authorized_but_unfilled_gap_units: report
                    .delayed_metrics
                    .time_weighted_mean_authorized_but_unfilled_gap_units,
                release_count: report.delayed_metrics.release_count,
                max_single_catch_up_units: report.delayed_metrics.max_single_catch_up_units,
            },
        },
        comparison: EvaluationComparisonDto {
            curve_immediate_time_weighted_tracking_error_units: comparison
                .curve_immediate_time_weighted_tracking_error_units,
            delayed_time_weighted_tracking_error_units: comparison
                .delayed_time_weighted_tracking_error_units,
            additional_delayed_tracking_error_units: comparison
                .additional_delayed_tracking_error_units,
            curve_immediate_max_tracking_error_units: comparison
                .curve_immediate_max_tracking_error_units,
            delayed_max_tracking_error_units: comparison.delayed_max_tracking_error_units,
            long_risk_increase_price: project_matched_price(&comparison.long_risk_increase_price),
            short_risk_increase_price: project_matched_price(&comparison.short_risk_increase_price),
        },
        replay_rows: report
            .delayed_decision_replay
            .steps
            .iter()
            .map(project_replay_row)
            .collect(),
    }
}

fn scenario_id(kind: ScenarioKind, direction: EvaluationDirectionDto) -> String {
    format!(
        "{}_{}",
        match kind {
            ScenarioKind::PriceAdvantageSweep => "price_advantage_sweep",
            ScenarioKind::StaleHold => "stale_hold",
        },
        match direction {
            EvaluationDirectionDto::Long => "long",
            EvaluationDirectionDto::Short => "short",
        }
    )
}

fn scenario_title(kind: ScenarioKind, direction: EvaluationDirectionDto) -> String {
    let kind = match kind {
        ScenarioKind::PriceAdvantageSweep => "价格优势释放",
        ScenarioKind::StaleHold => "等待超时释放",
    };
    let direction = match direction {
        EvaluationDirectionDto::Long => "做多",
        EvaluationDirectionDto::Short => "做空",
    };
    format!("{kind}（{direction}）")
}

fn project_instrument_fixture(
    fixture_id: EvaluationInstrumentFixtureId,
    fixture: &EvaluationInstrumentFixture,
    position_qty_per_exposure_unit: f64,
) -> EvaluationInstrumentFixtureDto {
    let spec = &fixture.instrument_spec;
    EvaluationInstrumentFixtureDto {
        fixture_id,
        base_asset: spec.base_asset().as_str().to_string(),
        quote_asset: spec.quote_asset().as_str().to_string(),
        notional_asset: spec.notional_asset().as_str().to_string(),
        price_tick: spec.price_tick(),
        price_precision: match spec.price_precision() {
            PricePrecision::FixedTick => EvaluationPricePrecisionDto::FixedTick,
            PricePrecision::SignificantFigures {
                max_decimals,
                significant_figures,
            } => EvaluationPricePrecisionDto::SignificantFigures {
                max_decimals,
                significant_figures,
            },
        },
        position_basis: match spec.position_basis() {
            PositionBasis::BaseAsset => EvaluationPositionBasisDto::BaseAsset,
            PositionBasis::Contracts { .. } => EvaluationPositionBasisDto::Contracts,
        },
        contract_value: spec.contract_value().map(|amount| amount.value()),
        settlement_asset: spec.settlement_asset().as_str().to_string(),
        position_qty_step: spec.position_qty_step().value(),
        min_position_qty: spec.min_position_qty().value(),
        min_notional: spec.min_notional().value(),
        maker_fee_rate: fixture.fee_rates.maker_fee_rate,
        taker_fee_rate: fixture.fee_rates.taker_fee_rate,
        position_qty_per_exposure_unit,
    }
}

fn project_execution(metrics: &ExecutionCostMetrics) -> ExecutionMetricsDto {
    ExecutionMetricsDto {
        long_risk_increase: RiskIncreaseFillMetricsDto {
            position_qty: metrics.long_risk_increase.position_qty,
            weighted_fill_price: metrics.long_risk_increase.weighted_fill_price,
        },
        short_risk_increase: RiskIncreaseFillMetricsDto {
            position_qty: metrics.short_risk_increase.position_qty,
            weighted_fill_price: metrics.short_risk_increase.weighted_fill_price,
        },
        turnover_notional: metrics.turnover_notional,
        estimated_taker_fee: metrics.estimated_taker_fee,
        fee_asset: metrics.fee_asset.clone(),
        max_abs_notional: metrics.max_abs_notional,
        max_margin_requirement: metrics.max_margin_requirement,
    }
}

fn project_matched_price(
    metrics: &MatchedRiskIncreasePriceComparison,
) -> MatchedPriceComparisonDto {
    MatchedPriceComparisonDto {
        curve_immediate_total_position_qty: metrics.curve_immediate_total_position_qty,
        delayed_total_position_qty: metrics.delayed_total_position_qty,
        matched_position_qty: metrics.matched_position_qty,
        curve_immediate_matched_weighted_price: metrics.curve_immediate_matched_weighted_price,
        delayed_matched_weighted_price: metrics.delayed_matched_weighted_price,
        favorable_price_improvement: metrics.favorable_price_improvement,
    }
}

fn project_replay_row(
    step: &poise_engine::decision_replay::DecisionReplayStepReport,
) -> ReplayRowDto {
    let decision = step.committed_decision.as_ref();
    ReplayRowDto {
        observed_at: step.observed_at.to_rfc3339(),
        action: project_action(&step.action).to_string(),
        strategy_price: step.strategy_price,
        simulated_position_qty: step.simulated_position_qty,
        simulated_position_exposure: step.simulated_position_exposure.0,
        observed_position_qty: step.observed_position_qty,
        observed_position_exposure: step.observed_position_exposure.0,
        signed_filled_position_qty: step.signed_filled_position_qty,
        policy_target: decision.map(|decision| decision.policy_target.0),
        risk_approved_target: decision.map(|decision| decision.risk_approved_target.0),
        acquisition_frontier: decision.and_then(|decision| {
            decision
                .acquisition_frontier
                .as_ref()
                .map(|frontier| frontier.0)
        }),
        execution_target: decision.map(|decision| decision.execution_target.0),
        reason: decision.map(|decision| decision_reason(decision.reason).to_string()),
        revision: decision.map(|decision| decision.revision),
        applied_releases: step.effects.iter().filter_map(project_release).collect(),
    }
}

fn project_action(action: &DecisionScenarioAction) -> &'static str {
    match action {
        DecisionScenarioAction::Market(MarketObservation::ExecutionQuote { .. }) => {
            "execution_quote"
        }
        DecisionScenarioAction::Market(MarketObservation::MarkPrice { .. }) => "mark_price",
        DecisionScenarioAction::Position(_) => "position",
        DecisionScenarioAction::Order(_) => "order",
        DecisionScenarioAction::Restart => "restart",
    }
}

fn project_release(effect: &DecisionReplayEffect) -> Option<AppliedReleaseDto> {
    let DecisionReplayEffect::RiskReleaseApplied {
        trigger,
        direction,
        frontier_before,
        frontier_after,
        release_units,
    } = effect
    else {
        return None;
    };
    Some(AppliedReleaseDto {
        trigger: match trigger {
            DecisionReplayReleaseTrigger::InitialRatio => EvaluationReleaseTriggerDto::InitialRatio,
            DecisionReplayReleaseTrigger::PriceAdvantage => {
                EvaluationReleaseTriggerDto::PriceAdvantage
            }
            DecisionReplayReleaseTrigger::StaleTimeout => EvaluationReleaseTriggerDto::StaleTimeout,
        },
        direction: match direction {
            RiskAcquisitionDirection::Long => EvaluationDirectionDto::Long,
            RiskAcquisitionDirection::Short => EvaluationDirectionDto::Short,
        },
        frontier_before: frontier_before.0,
        frontier_after: frontier_after.0,
        release_units: *release_units,
    })
}

fn decision_reason(reason: DecisionReason) -> &'static str {
    match reason {
        DecisionReason::WaitingMarketData => "waiting_market_data",
        DecisionReason::Paused => "paused",
        DecisionReason::AtExecutionTarget => "at_execution_target",
        DecisionReason::WaitingRiskRelease => "waiting_risk_release",
        DecisionReason::AccountCapacityBlocked => "account_capacity_blocked",
        DecisionReason::RiskInputInvalid => "risk_input_invalid",
        DecisionReason::BandPolicyBlocked => "band_policy_blocked",
        DecisionReason::PriceGateBlocked => "price_gate_blocked",
        DecisionReason::BelowRebalance => "below_rebalance",
        DecisionReason::BelowVenueMinimum => "below_venue_minimum",
        DecisionReason::CoveredByWorkingOrder => "covered_by_working_order",
        DecisionReason::RecoveryRequired => "recovery_required",
        DecisionReason::WaitingCancel => "waiting_cancel",
        DecisionReason::SubmitPlanned => "submit_planned",
    }
}

fn evaluation_error(message: impl Into<String>) -> CommandError {
    CommandError::new(CommandErrorKind::Evaluation, message)
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::RiskAcquisitionConfig;
    use serde_json::{Value, json};

    use crate::{
        commands::{EditableTrackFieldsPayload, TrackDraftPayload},
        error::CommandErrorKind,
    };

    use super::{
        EvaluationDirectionDto, EvaluationInstrumentFixtureId, EvaluationPositionBasisDto,
        EvaluationReleaseTriggerDto, ScenarioReportDto, WorkbenchEvaluationRequest,
        evaluate_workbench_draft,
    };

    #[test]
    fn request_requires_a_known_instrument_fixture() {
        let mut value =
            serde_json::to_value(request(EvaluationInstrumentFixtureId::BaseAssetV1)).unwrap();
        value.as_object_mut().unwrap().remove("instrument_fixture");
        assert!(serde_json::from_value::<WorkbenchEvaluationRequest>(value).is_err());

        let mut value =
            serde_json::to_value(request(EvaluationInstrumentFixtureId::BaseAssetV1)).unwrap();
        value["instrument_fixture"] = json!("guessed_from_venue");
        assert!(serde_json::from_value::<WorkbenchEvaluationRequest>(value).is_err());
    }

    #[test]
    fn report_serializes_one_camel_case_read_model_for_typescript() {
        let report =
            evaluate_workbench_draft(request(EvaluationInstrumentFixtureId::BaseAssetV1)).unwrap();
        let value = serde_json::to_value(report).unwrap();

        assert!(value.get("curvePoints").is_some());
        assert!(value.get("curve_points").is_none());
        assert!(value["instrumentFixture"].get("fixtureId").is_some());
        assert!(value["instrumentFixture"].get("fixture_id").is_none());
        assert!(
            value["scenarios"][0]
                .get("curveImmediateBenchmark")
                .is_some()
        );
        assert!(
            value["scenarios"][0]["replayRows"][0]
                .get("policyTarget")
                .is_some()
        );
        assert_no_snake_case_object_keys(&value);
    }

    #[test]
    fn adapter_builds_complete_rules_curve_and_directional_scenarios() {
        let report =
            evaluate_workbench_draft(request(EvaluationInstrumentFixtureId::BaseAssetV1)).unwrap();

        assert_eq!(
            report.instrument_fixture.fixture_id,
            EvaluationInstrumentFixtureId::BaseAssetV1
        );
        assert_eq!(
            report.instrument_fixture.position_basis,
            EvaluationPositionBasisDto::BaseAsset
        );
        assert_eq!(report.instrument_fixture.base_asset, "BTC");
        assert_eq!(report.instrument_fixture.notional_asset, "USDT");
        assert_eq!(report.instrument_fixture.settlement_asset, "USDT");
        assert!(report.instrument_fixture.position_qty_per_exposure_unit > 0.0);
        assert_eq!(report.curve_points.first().unwrap().target_exposure, 8.0);
        assert_eq!(report.curve_points.last().unwrap().target_exposure, -8.0);
        assert_eq!(report.scenarios.len(), 4);
        assert!(
            scenario(&report.scenarios, "price_advantage_sweep_long")
                .replay_rows
                .iter()
                .any(|row| {
                    row.policy_target.is_some()
                        && row.risk_approved_target.is_some()
                        && row.acquisition_frontier.is_some()
                        && row.execution_target.is_some()
                        && row.reason.is_some()
                        && row.revision.is_some()
                })
        );
    }

    #[test]
    fn initial_ratio_changes_initial_release_and_execution_target() {
        let low = report_with_risk(|risk| risk.initial_ratio = 0.25);
        let high = report_with_risk(|risk| risk.initial_ratio = 0.75);
        let low = scenario(&low.scenarios, "price_advantage_sweep_long");
        let high = scenario(&high.scenarios, "price_advantage_sweep_long");

        assert!(
            release_units(low, EvaluationReleaseTriggerDto::InitialRatio)
                < release_units(high, EvaluationReleaseTriggerDto::InitialRatio)
        );
        assert!(first_released_execution_target(low) < first_released_execution_target(high));
    }

    #[test]
    fn advantage_steps_changes_price_advantage_trigger() {
        let reached = report_with_risk(|risk| risk.advantage_steps = 2.0);
        let not_reached = report_with_risk(|risk| risk.advantage_steps = 8.0);

        assert!(has_trigger(
            scenario(&reached.scenarios, "price_advantage_sweep_long"),
            EvaluationReleaseTriggerDto::PriceAdvantage,
        ));
        assert!(!has_trigger(
            scenario(&not_reached.scenarios, "price_advantage_sweep_long"),
            EvaluationReleaseTriggerDto::PriceAdvantage,
        ));
    }

    #[test]
    fn min_release_steps_controls_the_minimum_release_clamp() {
        let small = report_with_risk(|risk| {
            risk.min_release_steps = 1.0;
            risk.max_release_steps = 8.0;
            risk.catchup_ratio = 0.1;
        });
        let large = report_with_risk(|risk| {
            risk.min_release_steps = 3.0;
            risk.max_release_steps = 8.0;
            risk.catchup_ratio = 0.1;
        });

        assert!(price_advantage_release(&small) < price_advantage_release(&large));
    }

    #[test]
    fn max_release_steps_controls_the_maximum_release_clamp() {
        let small = report_with_risk(|risk| {
            risk.min_release_steps = 1.0;
            risk.max_release_steps = 2.0;
            risk.catchup_ratio = 1.0;
        });
        let large = report_with_risk(|risk| {
            risk.min_release_steps = 1.0;
            risk.max_release_steps = 4.0;
            risk.catchup_ratio = 1.0;
        });

        assert!(price_advantage_release(&small) < price_advantage_release(&large));
    }

    #[test]
    fn catchup_ratio_changes_dynamic_release_and_backlog() {
        let small = report_with_risk(|risk| {
            risk.min_release_steps = 0.1;
            risk.max_release_steps = 8.0;
            risk.catchup_ratio = 0.25;
        });
        let large = report_with_risk(|risk| {
            risk.min_release_steps = 0.1;
            risk.max_release_steps = 8.0;
            risk.catchup_ratio = 0.75;
        });
        let small_scenario = scenario(&small.scenarios, "price_advantage_sweep_long");
        let large_scenario = scenario(&large.scenarios, "price_advantage_sweep_long");

        assert!(price_advantage_release(&small) < price_advantage_release(&large));
        assert!(
            small_scenario
                .delayed_decision_replay
                .backlog
                .max_backlog_units
                > large_scenario
                    .delayed_decision_replay
                    .backlog
                    .max_backlog_units
        );
    }

    #[test]
    fn stale_release_minutes_changes_waiting_horizon_and_trigger_time() {
        let early = report_with_risk(|risk| risk.stale_release_minutes = 30.0);
        let late = report_with_risk(|risk| risk.stale_release_minutes = 60.0);
        let early = scenario(&early.scenarios, "stale_hold_long");
        let late = scenario(&late.scenarios, "stale_hold_long");

        assert!(
            early.horizon.wall_clock_duration_seconds < late.horizon.wall_clock_duration_seconds
        );
        assert!(
            trigger_time(early, EvaluationReleaseTriggerDto::StaleTimeout)
                < trigger_time(late, EvaluationReleaseTriggerDto::StaleTimeout)
        );
    }

    #[test]
    fn zero_stale_threshold_disables_time_release_but_still_returns_a_report() {
        let report = report_with_risk(|risk| risk.stale_release_minutes = 0.0);
        let scenario = scenario(&report.scenarios, "stale_hold_long");

        assert_eq!(scenario.replay_rows.len(), 2);
        assert!(!has_trigger(
            scenario,
            EvaluationReleaseTriggerDto::StaleTimeout,
        ));
        assert_close(scenario.horizon.wall_clock_duration_seconds, 120.0);
    }

    #[test]
    fn short_only_draft_generates_only_non_empty_short_scenarios() {
        let mut request = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        request.draft.fields.long_exposure_units = 0.0;
        let report = evaluate_workbench_draft(request).unwrap();

        assert_eq!(report.scenarios.len(), 2);
        assert!(report.scenarios.iter().all(|scenario| {
            let first_price = scenario
                .replay_rows
                .iter()
                .filter_map(|row| row.strategy_price)
                .next()
                .unwrap();
            scenario.direction == EvaluationDirectionDto::Short
                && !scenario.replay_rows.is_empty()
                && scenario
                    .replay_rows
                    .iter()
                    .filter_map(|row| row.strategy_price)
                    .next_back()
                    .unwrap()
                    > first_price
        }));
        assert!(
            report
                .scenarios
                .iter()
                .flat_map(|scenario| &scenario.replay_rows)
                .any(|row| {
                    row.applied_releases
                        .iter()
                        .any(|release| release.direction == EvaluationDirectionDto::Short)
                })
        );
    }

    #[test]
    fn asymmetric_directional_scenarios_start_at_the_true_zero_target() {
        let mut request = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        request.draft.fields.long_exposure_units = 2.0;
        request.draft.fields.short_exposure_units = 8.0;
        let report = evaluate_workbench_draft(request).unwrap();
        let scenario = scenario(&report.scenarios, "price_advantage_sweep_long");

        assert_close(scenario.replay_rows[0].policy_target.unwrap(), 0.0);
        assert!(scenario.replay_rows[0].applied_releases.is_empty());
        assert!(scenario.replay_rows[1].policy_target.unwrap() > 0.0);
        assert!(
            scenario.replay_rows[1]
                .applied_releases
                .iter()
                .any(|release| release.trigger == EvaluationReleaseTriggerDto::InitialRatio)
        );
    }

    #[test]
    fn short_only_stale_release_occurs_at_backlog_threshold_not_before() {
        let mut request = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        request.draft.fields.long_exposure_units = 0.0;
        request.draft.fields.risk_acquisition.stale_release_minutes = 30.0;
        let report = evaluate_workbench_draft(request).unwrap();
        let scenario = scenario(&report.scenarios, "stale_hold_short");

        assert_close(scenario.replay_rows[0].policy_target.unwrap(), 0.0);
        assert!(scenario.replay_rows[2].applied_releases.is_empty());
        assert!(
            scenario.replay_rows[3]
                .applied_releases
                .iter()
                .any(|release| release.trigger == EvaluationReleaseTriggerDto::StaleTimeout)
        );
        assert_eq!(
            scenario.replay_rows[2].observed_at,
            "2026-01-01T00:30:59+00:00"
        );
        assert_eq!(
            scenario.replay_rows[3].observed_at,
            "2026-01-01T00:31:00+00:00"
        );
    }

    #[test]
    fn any_positive_capacity_generates_its_directional_scenarios() {
        let mut request = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        request.draft.fields.long_exposure_units = f64::EPSILON / 2.0;
        request.draft.fields.short_exposure_units = 0.0;

        let report = evaluate_workbench_draft(request).unwrap();

        assert_eq!(report.scenarios.len(), 2);
        assert!(
            report
                .scenarios
                .iter()
                .all(|scenario| scenario.direction == EvaluationDirectionDto::Long)
        );
    }

    #[test]
    fn inverse_fixture_uses_engine_quantity_notional_margin_and_fee_asset() {
        let mut request = request(EvaluationInstrumentFixtureId::InverseContractV1);
        request.draft.fields.symbol = "BTCUSD_PERP".to_string();
        request.draft.fields.lower_price = 60_000.0;
        request.draft.fields.upper_price = 70_000.0;
        request.draft.fields.notional_per_unit = 300.0;
        request.draft.fields.max_notional = 2_400.0;
        request.draft.fields.leverage = 3;
        let report = evaluate_workbench_draft(request).unwrap();
        let scenario = scenario(&report.scenarios, "price_advantage_sweep_long");
        let execution = &scenario.curve_immediate_benchmark.execution;

        assert_eq!(
            report.instrument_fixture.position_basis,
            EvaluationPositionBasisDto::Contracts
        );
        assert_eq!(report.instrument_fixture.contract_value, Some(100.0));
        assert_eq!(report.instrument_fixture.notional_asset, "USD");
        assert_eq!(report.instrument_fixture.settlement_asset, "BTC");
        assert_close(
            report.instrument_fixture.position_qty_per_exposure_unit,
            3.0,
        );
        assert_close(
            execution.max_margin_requirement,
            execution.max_abs_notional / 3.0,
        );
        assert_eq!(execution.fee_asset, "BTC");
    }

    #[test]
    fn inverse_projection_matches_direct_engine_evaluator() {
        let mut request = request(EvaluationInstrumentFixtureId::InverseContractV1);
        request.draft.fields.symbol = "BTCUSD_PERP".to_string();
        request.draft.fields.lower_price = 60_000.0;
        request.draft.fields.upper_price = 70_000.0;
        request.draft.fields.notional_per_unit = 300.0;
        request.draft.fields.max_notional = 2_400.0;
        request.draft.fields.leverage = 3;

        let projected = evaluate_workbench_draft(request.clone()).unwrap();
        let definition = super::build_definition(
            crate::config_document::TrackDraft::try_from(request.draft).unwrap(),
            &request.exchange_venue,
        )
        .unwrap();
        let fixture =
            super::instrument_fixture(request.instrument_fixture, definition.instrument().clone());
        let raw_input = super::build_scenario_input(
            &definition,
            &fixture,
            3,
            super::ScenarioKind::PriceAdvantageSweep,
            EvaluationDirectionDto::Long,
        )
        .unwrap();
        let raw = poise_engine::decision_evaluation::run_decision_evaluation(&raw_input).unwrap();
        let projected = scenario(&projected.scenarios, "price_advantage_sweep_long");

        assert_execution_matches(
            &projected.curve_immediate_benchmark.execution,
            &raw.curve_immediate_metrics,
        );
        assert_execution_matches(
            &projected.delayed_decision_replay.execution,
            &raw.delayed_metrics.execution,
        );
    }

    #[test]
    fn evaluation_errors_are_explicit_for_invalid_venue_and_config() {
        let mut invalid_venue = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        invalid_venue.exchange_venue = "guessed".to_string();
        let error = evaluate_workbench_draft(invalid_venue).unwrap_err();
        assert_eq!(error.kind, CommandErrorKind::Evaluation);
        assert!(error.message.contains("exchange_venue"));

        let mut invalid_config = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        invalid_config.draft.fields.upper_price = invalid_config.draft.fields.lower_price;
        let error = evaluate_workbench_draft(invalid_config).unwrap_err();
        assert_eq!(error.kind, CommandErrorKind::Evaluation);
        assert!(error.message.contains("lower_price"));

        let mut invalid_horizon = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        invalid_horizon
            .draft
            .fields
            .risk_acquisition
            .stale_release_minutes = f64::MAX;
        let error = evaluate_workbench_draft(invalid_horizon).unwrap_err();
        assert_eq!(error.kind, CommandErrorKind::Evaluation);
        assert!(error.message.contains("stale_release_minutes"));
    }

    fn request(instrument_fixture: EvaluationInstrumentFixtureId) -> WorkbenchEvaluationRequest {
        WorkbenchEvaluationRequest {
            draft: TrackDraftPayload {
                draft_id: "fixture-draft".to_string(),
                fields: EditableTrackFieldsPayload {
                    track_id: "fixture".to_string(),
                    symbol: "BTCUSDT".to_string(),
                    lower_price: 90.0,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 100.0,
                    max_notional: 1_600.0,
                    min_rebalance_units: 1.0,
                    leverage: 10,
                    out_of_band_policy: json!("freeze"),
                    daily_loss_limit: 1_000.0,
                    total_loss_limit: 5_000.0,
                    shape_family: "linear".to_string(),
                    risk_acquisition: RiskAcquisitionConfig::default(),
                },
                load_issues: Vec::new(),
            },
            exchange_venue: "binance".to_string(),
            instrument_fixture,
        }
    }

    fn report_with_risk(
        update: impl FnOnce(&mut RiskAcquisitionConfig),
    ) -> super::WorkbenchEvaluationReportDto {
        let mut request = request(EvaluationInstrumentFixtureId::BaseAssetV1);
        update(&mut request.draft.fields.risk_acquisition);
        evaluate_workbench_draft(request).unwrap()
    }

    fn scenario<'a>(scenarios: &'a [ScenarioReportDto], id: &str) -> &'a ScenarioReportDto {
        scenarios
            .iter()
            .find(|scenario| scenario.id == id)
            .unwrap_or_else(|| panic!("scenario `{id}` missing"))
    }

    fn has_trigger(scenario: &ScenarioReportDto, trigger: EvaluationReleaseTriggerDto) -> bool {
        scenario.replay_rows.iter().any(|row| {
            row.applied_releases
                .iter()
                .any(|release| release.trigger == trigger)
        })
    }

    fn release_units(scenario: &ScenarioReportDto, trigger: EvaluationReleaseTriggerDto) -> f64 {
        scenario
            .replay_rows
            .iter()
            .flat_map(|row| &row.applied_releases)
            .find(|release| release.trigger == trigger)
            .unwrap_or_else(|| panic!("release trigger {trigger:?} missing"))
            .release_units
    }

    fn price_advantage_release(report: &super::WorkbenchEvaluationReportDto) -> f64 {
        release_units(
            scenario(&report.scenarios, "price_advantage_sweep_long"),
            EvaluationReleaseTriggerDto::PriceAdvantage,
        )
    }

    fn first_released_execution_target(scenario: &ScenarioReportDto) -> f64 {
        scenario
            .replay_rows
            .iter()
            .find(|row| {
                row.applied_releases
                    .iter()
                    .any(|release| release.trigger == EvaluationReleaseTriggerDto::InitialRatio)
            })
            .and_then(|row| row.execution_target)
            .unwrap()
    }

    fn trigger_time(scenario: &ScenarioReportDto, trigger: EvaluationReleaseTriggerDto) -> &str {
        scenario
            .replay_rows
            .iter()
            .find(|row| {
                row.applied_releases
                    .iter()
                    .any(|release| release.trigger == trigger)
            })
            .map(|row| row.observed_at.as_str())
            .unwrap_or_else(|| panic!("release trigger {trigger:?} missing"))
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {actual} to be close to {expected}"
        );
    }

    fn assert_no_snake_case_object_keys(value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(!key.contains('_'), "wire field `{key}` must be camelCase");
                    assert_no_snake_case_object_keys(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    assert_no_snake_case_object_keys(value);
                }
            }
            _ => {}
        }
    }

    fn assert_execution_matches(
        projected: &super::ExecutionMetricsDto,
        raw: &poise_engine::decision_evaluation::ExecutionCostMetrics,
    ) {
        assert_close(
            projected.long_risk_increase.position_qty,
            raw.long_risk_increase.position_qty,
        );
        assert_eq!(
            projected.long_risk_increase.weighted_fill_price,
            raw.long_risk_increase.weighted_fill_price,
        );
        assert_close(
            projected.short_risk_increase.position_qty,
            raw.short_risk_increase.position_qty,
        );
        assert_eq!(
            projected.short_risk_increase.weighted_fill_price,
            raw.short_risk_increase.weighted_fill_price,
        );
        assert_close(projected.turnover_notional, raw.turnover_notional);
        assert_close(projected.estimated_taker_fee, raw.estimated_taker_fee);
        assert_eq!(projected.fee_asset, raw.fee_asset);
        assert_close(projected.max_abs_notional, raw.max_abs_notional);
        assert_close(projected.max_margin_requirement, raw.max_margin_requirement);
    }
}
