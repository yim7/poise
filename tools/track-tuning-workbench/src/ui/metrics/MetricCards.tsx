import type {
  WorkbenchEvaluationLane,
  WorkbenchEvaluationReport,
  WorkbenchReplayRow,
  WorkbenchRulesFixtureId,
  WorkbenchScenarioReport,
} from '@/app/workbenchBridge';
import type { TrackEvaluationState } from '@/ui/app/useTrackEvaluation';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { StatusBadge } from '@/ui/common/StatusBadge';

export interface MetricCardsPriceStatus {
  tone: 'accent' | 'warning' | 'danger';
  badge: string;
  note: string;
}

export interface MetricCardsProps {
  evaluation: TrackEvaluationState;
  rulesFixture: WorkbenchRulesFixtureId | null;
  onRulesFixtureChange(fixture: WorkbenchRulesFixtureId | null): void;
  selectedScenarioId: string;
  onSelectedScenarioChange(scenarioId: string): void;
  priceStatus: MetricCardsPriceStatus;
}

export function MetricCards({
  evaluation,
  rulesFixture,
  onRulesFixtureChange,
  selectedScenarioId,
  onSelectedScenarioChange,
  priceStatus,
}: MetricCardsProps) {
  const report = evaluation.report;
  const scenario = report?.scenarios.find((item) => item.id === selectedScenarioId)
    ?? report?.scenarios[0]
    ?? null;

  return (
    <section className="workbench-panel workbench-panel--metrics" aria-label="关键指标区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">Rust decision evaluator</p>
          <h2 className="workbench-panel__title">曲线立即执行与延迟决策对照</h2>
        </div>
        <div className="metric-cards__status">
          <StatusBadge tone={priceStatus.tone}>{priceStatus.badge}</StatusBadge>
          <p className="metric-cards__status-note">{priceStatus.note}</p>
        </div>
      </div>

      <div className="field-grid field-grid--two evaluation-controls">
        <label className="field">
          <span className="field__label">评估规则 fixture</span>
          <select
            className="field__input"
            aria-label="评估规则 fixture"
            value={rulesFixture ?? ''}
            onChange={(event) => {
              onRulesFixtureChange(
                event.target.value === ''
                  ? null
                  : event.target.value as WorkbenchRulesFixtureId,
              );
            }}
          >
            <option value="">请选择完整规则</option>
            <option value="base_asset_v1">base_asset_v1</option>
            <option value="inverse_contract_v1">inverse_contract_v1</option>
          </select>
        </label>
        {report && report.scenarios.length > 0 ? (
          <label className="field">
            <span className="field__label">评估场景</span>
            <select
              className="field__input"
              aria-label="评估场景"
              value={scenario?.id ?? ''}
              onChange={(event) => onSelectedScenarioChange(event.target.value)}
            >
              {report.scenarios.map((item) => (
                <option key={item.id} value={item.id}>{item.title}</option>
              ))}
            </select>
          </label>
        ) : null}
      </div>

      <EvaluationStateNotice evaluation={evaluation} />
      {report ? <RulesSummary report={report} /> : null}
      {report && scenario ? <ScenarioComparison scenario={scenario} /> : null}
    </section>
  );
}

function EvaluationStateNotice({ evaluation }: { evaluation: TrackEvaluationState }) {
  switch (evaluation.status) {
    case 'unavailable':
    case 'idle':
    case 'blocked':
      return <InlineNotice tone="warning">{evaluation.message}</InlineNotice>;
    case 'loading':
      return <InlineNotice>Rust evaluator 正在运行…</InlineNotice>;
    case 'error':
      return <InlineNotice tone="warning" title="Rust evaluator 失败">{evaluation.message}</InlineNotice>;
    case 'ready':
      return null;
  }
}

function RulesSummary({ report }: { report: WorkbenchEvaluationReport }) {
  const { rules, assumptions } = report;
  return (
    <div className="evaluation-summary">
      <div className="metric-cards metric-cards--compact">
        <Metric title="规则 fixture" value={rules.fixtureId} note={rules.quantityKind} />
        <Metric
          title="数量换算"
          value={formatNumber(rules.nativeQtyPerExposureUnit)}
          note={`native qty / exposure unit · quantity step ${formatNumber(rules.quantityStep)}`}
        />
        <Metric
          title="价格规则"
          value={`tick ${formatNumber(rules.priceTick)}`}
          note={formatPricePrecision(rules.pricePrecision)}
        />
        <Metric
          title="最小交易"
          value={`qty ${formatNumber(rules.minQty)}`}
          note={`notional ${formatNumber(rules.minNotional)}`}
        />
        <Metric
          title="合约与结算"
          value={rules.contractNotional === null
            ? 'contract notional --'
            : `contract notional ${formatNumber(rules.contractNotional)}`}
          note={`settlement ${rules.settlementAsset}`}
        />
        <Metric
          title="费用假设"
          value={`taker ${formatNumber(rules.takerFeeRate)}`}
          note={`maker ${formatNumber(rules.makerFeeRate)}`}
        />
      </div>
      <InlineNotice title="评估假设">
        <p>{assumptions.rulesNotice}</p>
        <p>{assumptions.fillModelNotice}</p>
      </InlineNotice>
    </div>
  );
}

function ScenarioComparison({ scenario }: { scenario: WorkbenchScenarioReport }) {
  return (
    <div className="evaluation-scenario">
      <div className="evaluation-scenario__header">
        <div>
          <p className="workbench-panel__eyebrow">{scenario.id}</p>
          <h3>{scenario.title}</h3>
        </div>
        <StatusBadge tone="neutral">{scenario.direction}</StatusBadge>
      </div>
      <p className="metric-card__source">
        {scenario.assumptions.deterministicFillNotice} · live {formatNumber(scenario.horizon.liveDurationSeconds)}s
      </p>

      <div className="evaluation-lanes">
        <article className="evaluation-lane" role="region" aria-label="Curve-immediate benchmark">
          <p className="workbench-panel__eyebrow">Curve-immediate benchmark</p>
          <h3>曲线目标立即执行</h3>
          <LaneMetrics lane={scenario.curveImmediateBenchmark} direction={scenario.direction} />
        </article>
        <article className="evaluation-lane" role="region" aria-label="Delayed decision replay">
          <p className="workbench-panel__eyebrow">Delayed decision replay</p>
          <h3>风险获取延迟决策</h3>
          <LaneMetrics lane={scenario.delayedDecisionReplay} direction={scenario.direction} />
          <div className="metric-cards metric-cards--compact">
            <Metric
              title="最大 backlog"
              value={formatNumber(scenario.delayedDecisionReplay.backlog.maxBacklogUnits)}
              note={`均值 ${formatNumber(scenario.delayedDecisionReplay.backlog.timeWeightedMeanBacklogUnits)}`}
            />
            <Metric
              title="授权未成交 gap"
              value={formatNumber(scenario.delayedDecisionReplay.backlog.maxAuthorizedButUnfilledGapUnits)}
              note={`均值 ${formatNumber(scenario.delayedDecisionReplay.backlog.timeWeightedMeanAuthorizedButUnfilledGapUnits)}`}
            />
            <Metric
              title="已应用 release"
              value={String(scenario.delayedDecisionReplay.backlog.releaseCount)}
              note={`最大 catch-up ${formatNumber(scenario.delayedDecisionReplay.backlog.maxSingleCatchUpUnits)}`}
            />
          </div>
          <ReplayRows rows={scenario.replayRows} />
        </article>
      </div>

      <InlineNotice title="两条 lane 的差异">
        <MatchedPriceSummary scenario={scenario} />
      </InlineNotice>
    </div>
  );
}

function LaneMetrics({
  lane,
  direction,
}: {
  lane: WorkbenchEvaluationLane;
  direction: WorkbenchScenarioReport['direction'];
}) {
  const riskIncrease = direction === 'long'
    ? lane.execution.longRiskIncrease
    : lane.execution.shortRiskIncrease;
  return (
    <div className="metric-cards metric-cards--compact">
      <Metric
        title="最大绝对名义"
        value={formatNumber(lane.execution.maxAbsNotional)}
        note={`margin ${formatNumber(lane.execution.maxMarginRequirement)}`}
      />
      <Metric
        title="time-weighted tracking error"
        value={formatNumber(lane.tracking.timeWeightedTrackingErrorUnits)}
        note={`max ${formatNumber(lane.tracking.maxTrackingErrorUnits)}`}
      />
      <Metric
        title={`${direction} 新增风险成交`}
        value={`native qty ${formatNumber(riskIncrease.nativeQty)}`}
        note={`weighted price ${formatNullable(riskIncrease.weightedFillPrice)}`}
      />
      <Metric
        title="turnover / fee"
        value={formatNumber(lane.execution.turnoverNotional)}
        note={`${formatNumber(lane.execution.estimatedTakerFee)} ${lane.execution.feeAsset}`}
      />
    </div>
  );
}

function MatchedPriceSummary({ scenario }: { scenario: WorkbenchScenarioReport }) {
  const matched = scenario.direction === 'long'
    ? scenario.comparison.longRiskIncreasePrice
    : scenario.comparison.shortRiskIncreasePrice;
  return (
    <div className="evaluation-matched-price">
      <p>
        matched native qty {formatNumber(matched.matchedNativeQty)} · immediate matched weighted price
        {' '}{formatNullable(matched.curveImmediateMatchedWeightedPrice)} · delayed matched weighted price
        {' '}{formatNullable(matched.delayedMatchedWeightedPrice)} · favorable improvement
        {' '}{formatNullable(matched.favorablePriceImprovement)}
      </p>
      <p>
        delayed 额外 time-weighted tracking error
        {' '}{formatNumber(scenario.comparison.additionalDelayedTrackingErrorUnits)} units。
        价格改善只比较共同新增风险成交量，不是 PnL 预测。
      </p>
    </div>
  );
}

function ReplayRows({ rows }: { rows: WorkbenchReplayRow[] }) {
  const decisionRows = rows.filter((row) => row.revision !== null || row.reason !== null);
  if (decisionRows.length === 0) {
    return <p className="metric-card__source">当前场景没有可见 decision row。</p>;
  }
  return (
    <div className="evaluation-replay" aria-label="Delayed replay rows">
      {decisionRows.map((row, index) => (
        <article className="evaluation-replay__row" key={`${row.observedAt}-${index}`}>
          <div className="evaluation-replay__meta">
            <span>{row.observedAt}</span>
            <span>{row.reason ?? 'decision unavailable'}</span>
            <span>{row.revision === null ? 'revision --' : `revision ${row.revision}`}</span>
          </div>
          <div className="evaluation-targets">
            <span>policy {formatNullable(row.policyTarget)}</span>
            <span>risk approved {formatNullable(row.riskApprovedTarget)}</span>
            <span>frontier {formatNullable(row.acquisitionFrontier)}</span>
            <span>execution {formatNullable(row.executionTarget)}</span>
          </div>
          {row.appliedReleases.map((release, releaseIndex) => (
            <p className="metric-card__source" key={`${release.trigger}-${releaseIndex}`}>
              {release.trigger} · {release.direction} · release {formatNumber(release.releaseUnits)}
            </p>
          ))}
        </article>
      ))}
    </div>
  );
}

function Metric({ title, value, note }: { title: string; value: string; note: string }) {
  return (
    <article className="metric-card">
      <p className="metric-card__title">{title}</p>
      <p className="metric-card__primary">{value}</p>
      <p className="metric-card__secondary">{note}</p>
    </article>
  );
}

function formatNullable(value: number | null) {
  return value === null ? '--' : formatNumber(value);
}

function formatNumber(value: number) {
  return value.toLocaleString('en-US', {
    minimumFractionDigits: 4,
    maximumFractionDigits: 4,
  });
}

function formatPricePrecision(
  precision: WorkbenchEvaluationReport['rules']['pricePrecision'],
) {
  if (precision.kind === 'fixed_tick') {
    return 'fixed tick';
  }
  return `${precision.significantFigures} significant figures · max ${precision.maxDecimals} decimals`;
}
