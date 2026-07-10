import type { WorkbenchEvaluationReport } from '@/app/workbenchBridge';

export interface TrackWorkbenchChartProps {
  report: WorkbenchEvaluationReport | null;
  selectedScenarioId: string;
}

const VIEWBOX_WIDTH = 920;
const VIEWBOX_HEIGHT = 372;
const PADDING_X = 52;
const PADDING_Y = 28;

export function TrackWorkbenchChart({
  report,
  selectedScenarioId,
}: TrackWorkbenchChartProps) {
  const scenario = report?.scenarios.find((item) => item.id === selectedScenarioId)
    ?? report?.scenarios[0]
    ?? null;
  if (!report || report.curvePoints.length === 0 || !scenario) {
    return (
      <section className="workbench-panel workbench-panel--chart" aria-label="主图区">
        <div className="workbench-panel__header">
          <div>
            <p className="workbench-panel__eyebrow">Rust evaluator 图表</p>
            <h2 className="workbench-panel__title">等待 Rust curve points</h2>
          </div>
        </div>
        <div className="empty-state empty-state--wide">
          <p className="empty-state__title">当前没有可展示的 evaluator report</p>
          <p className="empty-state__body">图表不会在 TypeScript 中重算曲线或决策。</p>
        </div>
      </section>
    );
  }

  const replayPoints = scenario.replayRows.flatMap((row) =>
    row.strategyPrice === null
      ? []
      : [{ price: row.strategyPrice, exposure: row.simulatedPositionExposure }]);
  const prices = [
    ...report.curvePoints.map((point) => point.price),
    ...replayPoints.map((point) => point.price),
  ];
  const exposures = [
    0,
    ...report.curvePoints.map((point) => point.targetExposure),
    ...replayPoints.map((point) => point.exposure),
  ];
  const minPrice = Math.min(...prices);
  const maxPrice = Math.max(...prices);
  const minExposure = Math.min(...exposures);
  const maxExposure = Math.max(...exposures);
  const width = VIEWBOX_WIDTH - PADDING_X * 2;
  const height = VIEWBOX_HEIGHT - PADDING_Y * 2;
  const toX = (price: number) =>
    PADDING_X + ((price - minPrice) / Math.max(maxPrice - minPrice, Number.EPSILON)) * width;
  const toY = (exposure: number) =>
    PADDING_Y + height
      - ((exposure - minExposure) / Math.max(maxExposure - minExposure, Number.EPSILON)) * height;
  const curvePath = linePath(
    report.curvePoints.map((point) => ({ x: toX(point.price), y: toY(point.targetExposure) })),
  );
  const replayPath = linePath(
    replayPoints.map((point) => ({ x: toX(point.price), y: toY(point.exposure) })),
  );

  return (
    <section className="workbench-panel workbench-panel--chart" aria-label="主图区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">Rust evaluator 图表</p>
          <h2 className="workbench-panel__title">Rust curve points 与 delayed replay</h2>
        </div>
      </div>
      <div className="chart-card">
        <svg
          className="track-workbench-chart"
          viewBox={`0 0 ${VIEWBOX_WIDTH} ${VIEWBOX_HEIGHT}`}
          role="img"
          aria-label="Rust evaluator 曲线与 replay"
        >
          <rect
            className="track-workbench-chart__frame"
            x="1"
            y="1"
            width={VIEWBOX_WIDTH - 2}
            height={VIEWBOX_HEIGHT - 2}
            rx="28"
          />
          <line
            className="track-workbench-chart__zero"
            x1={PADDING_X}
            x2={VIEWBOX_WIDTH - PADDING_X}
            y1={toY(0)}
            y2={toY(0)}
          />
          <path className="track-workbench-chart__curve" d={curvePath} />
          {replayPath ? (
            <path className="track-workbench-chart__risk-line" d={replayPath} />
          ) : null}
        </svg>
        <div className="chart-card__legend">
          <div className="chart-card__legend-item">
            <span className="chart-card__swatch chart-card__swatch--zero" />
            Rust curve_points
          </div>
          <div className="chart-card__legend-item">
            <span className="chart-card__swatch chart-card__swatch--risk" />
            delayed simulated position
          </div>
        </div>
      </div>
    </section>
  );
}

function linePath(points: Array<{ x: number; y: number }>) {
  return points
    .map((point, index) => `${index === 0 ? 'M' : 'L'} ${point.x} ${point.y}`)
    .join(' ');
}
