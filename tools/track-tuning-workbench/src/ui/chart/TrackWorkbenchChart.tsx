import type { TrackDraftParsedSnapshot } from '@/domain/trackDraft';
import type { TrackMetrics } from '@/domain/trackMetrics';

export interface TrackWorkbenchChartProps {
  snapshot: TrackDraftParsedSnapshot | null;
  metrics: TrackMetrics | null;
}

const VIEWBOX_WIDTH = 920;
const VIEWBOX_HEIGHT = 372;
const PADDING_X = 52;
const PADDING_Y = 28;

export function TrackWorkbenchChart({
  snapshot,
  metrics,
}: TrackWorkbenchChartProps) {
  if (!snapshot || !metrics) {
    return (
      <section className="workbench-panel workbench-panel--chart" aria-label="主图区">
        <div className="workbench-panel__header">
          <div>
            <p className="workbench-panel__eyebrow">主图</p>
            <h2 className="workbench-panel__title">等待可用曲线</h2>
          </div>
        </div>
        <div className="empty-state empty-state--wide">
          <p className="empty-state__title">需要一个合法的 Track 才能绘图</p>
          <p className="empty-state__body">
            价格带、曲线形状、当前价格、零仓目标点和风险方向都会集中在这里显示。
          </p>
        </div>
      </section>
    );
  }

  const prices = metrics.curve.points.map((point) => point.price);
  const exposures = metrics.curve.points.map((point) => point.targetExposure);
  exposures.push(0);

  const minPrice = Math.min(...prices);
  const maxPrice = Math.max(...prices);
  const minExposure = Math.min(...exposures);
  const maxExposure = Math.max(...exposures);

  const width = VIEWBOX_WIDTH - PADDING_X * 2;
  const height = VIEWBOX_HEIGHT - PADDING_Y * 2;

  const toX = (price: number) =>
    PADDING_X + ((price - minPrice) / Math.max(maxPrice - minPrice, Number.EPSILON)) * width;
  const toY = (exposure: number) =>
    PADDING_Y + height - ((exposure - minExposure) / Math.max(maxExposure - minExposure, Number.EPSILON)) * height;

  const curvePath = metrics.curve.points
    .map((point, index) => `${index === 0 ? 'M' : 'L'} ${toX(point.price)} ${toY(point.targetExposure)}`)
    .join(' ');

  const zeroLineY = toY(0);
  const currentPriceX = toX(snapshot.ui.quotePrice);
  const zeroTargetX = toX(metrics.zeroTargetPrice);
  const minStepLeftX = toX(metrics.minStepRoundTrip.triggerPrice.lower);
  const minStepRightX = toX(metrics.minStepRoundTrip.triggerPrice.upper);
  const riskStartX = currentPriceX;
  const riskEndX = resolveRiskEdgeX(metrics, toX);

  return (
    <section className="workbench-panel workbench-panel--chart" aria-label="主图区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">主图</p>
          <h2 className="workbench-panel__title">价格带、仓位曲线与风险方向</h2>
        </div>
      </div>

      <div className="chart-card">
        <svg
          className="track-workbench-chart"
          viewBox={`0 0 ${VIEWBOX_WIDTH} ${VIEWBOX_HEIGHT}`}
          role="img"
          aria-label="Track 调参主图"
        >
          <defs>
            <linearGradient id="curve-area" x1="0%" x2="0%" y1="0%" y2="100%">
              <stop offset="0%" stopColor="var(--color-chart-fill)" stopOpacity="0.34" />
              <stop offset="100%" stopColor="var(--color-chart-fill)" stopOpacity="0.04" />
            </linearGradient>
            <marker
              id="risk-arrow"
              markerWidth="10"
              markerHeight="10"
              refX="8"
              refY="5"
              orient="auto"
            >
              <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--color-danger-strong)" />
            </marker>
          </defs>

          <rect
            className="track-workbench-chart__frame"
            x="1"
            y="1"
            width={VIEWBOX_WIDTH - 2}
            height={VIEWBOX_HEIGHT - 2}
            rx="28"
          />
          <rect
            className="track-workbench-chart__band"
            x={PADDING_X}
            y={PADDING_Y}
            width={width}
            height={height}
            rx="22"
          />
          <line
            className="track-workbench-chart__zero"
            x1={PADDING_X}
            x2={VIEWBOX_WIDTH - PADDING_X}
            y1={zeroLineY}
            y2={zeroLineY}
          />

          <path
            className="track-workbench-chart__curve-area"
            d={`${curvePath} L ${toX(maxPrice)} ${zeroLineY} L ${toX(minPrice)} ${zeroLineY} Z`}
            fill="url(#curve-area)"
          />
          <path className="track-workbench-chart__curve" d={curvePath} />

          <line
            className="track-workbench-chart__marker track-workbench-chart__marker--current"
            x1={currentPriceX}
            x2={currentPriceX}
            y1={PADDING_Y}
            y2={VIEWBOX_HEIGHT - PADDING_Y}
          />
          <line
            className="track-workbench-chart__marker track-workbench-chart__marker--zero-target"
            x1={zeroTargetX}
            x2={zeroTargetX}
            y1={PADDING_Y}
            y2={VIEWBOX_HEIGHT - PADDING_Y}
          />
          <line
            className="track-workbench-chart__risk-line"
            x1={riskStartX}
            x2={riskEndX}
            y1={VIEWBOX_HEIGHT - PADDING_Y / 2}
            y2={VIEWBOX_HEIGHT - PADDING_Y / 2}
            markerEnd="url(#risk-arrow)"
          />
          <line
            className="track-workbench-chart__step-line"
            x1={minStepLeftX}
            x2={minStepRightX}
            y1={PADDING_Y / 1.6}
            y2={PADDING_Y / 1.6}
          />
          <circle
            className="track-workbench-chart__step-dot"
            cx={minStepLeftX}
            cy={PADDING_Y / 1.6}
            r="4"
          />
          <circle
            className="track-workbench-chart__step-dot"
            cx={minStepRightX}
            cy={PADDING_Y / 1.6}
            r="4"
          />

          <text className="track-workbench-chart__label" x={currentPriceX + 10} y={PADDING_Y + 18}>
            当前价格
          </text>
          <text className="track-workbench-chart__label" x={zeroTargetX + 10} y={PADDING_Y + 38}>
            零仓目标点
          </text>
          <text className="track-workbench-chart__label" x={minStepLeftX} y={PADDING_Y / 1.6 - 10}>
            最小步长
          </text>
        </svg>

        <div className="chart-card__legend">
          <div className="chart-card__legend-item">
            <span className="chart-card__swatch chart-card__swatch--current" />
            当前价格 {snapshot.ui.quotePrice.toFixed(2)}
          </div>
          <div className="chart-card__legend-item">
            <span className="chart-card__swatch chart-card__swatch--zero" />
            零仓目标点 {metrics.zeroTargetPrice.toFixed(2)}
          </div>
          <div className="chart-card__legend-item">
            <span className="chart-card__swatch chart-card__swatch--risk" />
            风险方向 {describeRisk(metrics.currentPriceRiskEdge)}
          </div>
        </div>
      </div>
    </section>
  );
}

function resolveRiskEdgeX(
  metrics: TrackMetrics,
  toX: (price: number) => number,
) {
  if (metrics.currentPriceRiskEdge.mode === 'single') {
    return toX(metrics.currentPriceRiskEdge.edge.boundaryPrice);
  }

  const lower = toX(metrics.currentPriceRiskEdge.lower.boundaryPrice);
  const upper = toX(metrics.currentPriceRiskEdge.upper.boundaryPrice);
  return Math.abs(upper - toX(metrics.zeroTargetPrice)) > Math.abs(lower - toX(metrics.zeroTargetPrice))
    ? upper
    : lower;
}

function describeRisk(edge: TrackMetrics['currentPriceRiskEdge']) {
  if (edge.mode === 'single') {
    return edge.edge.boundary === 'below' ? '朝下边缘' : '朝上边缘';
  }

  return '双向观察';
}
