import type { TrackDraftParsedSnapshot } from '@/domain/trackDraft';
import type { TrackMetrics } from '@/domain/trackMetrics';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { StatusBadge } from '@/ui/common/StatusBadge';

export interface MetricCardsPriceStatus {
  tone: 'accent' | 'warning' | 'danger';
  badge: string;
  note: string;
}

export interface MetricCardsProps {
  snapshot: TrackDraftParsedSnapshot | null;
  metrics: TrackMetrics | null;
  priceStatus: MetricCardsPriceStatus;
}

interface MetricCardDefinition {
  title: string;
  primary: string;
  secondary: string;
  source: string;
}

export function MetricCards({
  snapshot,
  metrics,
  priceStatus,
}: MetricCardsProps) {
  if (!snapshot || !metrics) {
    return (
      <section className="workbench-panel workbench-panel--metrics" aria-label="关键指标区">
        <div className="workbench-panel__header">
          <div>
            <p className="workbench-panel__eyebrow">关键指标</p>
            <h2 className="workbench-panel__title">先选择一个可试算的 Track</h2>
          </div>
        </div>
        <div className="empty-state empty-state--wide">
          <p className="empty-state__title">当前还没有可计算的指标</p>
          <p className="empty-state__body">
            当价格和必填数字字段合法后，这里会展示当前价格、最小步长、风险边缘和零仓目标点等关键结果。
          </p>
        </div>
      </section>
    );
  }

  const minStepDownPrice = metrics.minStepRoundTrip.triggerPrice.lower;
  const minStepUpPrice = metrics.minStepRoundTrip.triggerPrice.upper;

  const cards: MetricCardDefinition[] = [
    {
      title: '当前价格',
      primary: formatPrice(snapshot.ui.quotePrice),
      secondary: `${snapshot.additional.symbol} · ${formatBandStatus(metrics.bandStatus.kind)}`,
      source: '当前试算锚点',
    },
    {
      title: '最小步长对应价格',
      primary: `下 ${formatPrice(minStepDownPrice)} / 上 ${formatPrice(minStepUpPrice)}`,
      secondary: `最小位移 ${formatSigned(metrics.minStepRoundTrip.priceMove.lower)} / ${formatSigned(
        metrics.minStepRoundTrip.priceMove.upper,
      )}`,
      source: '按 min_rebalance_units 反推',
    },
    {
      title: '每步理论净利',
      primary: `下 ${formatCurrency(metrics.minStepRoundTrip.netProfit.lower)} / 上 ${formatCurrency(
        metrics.minStepRoundTrip.netProfit.upper,
      )}`,
      secondary: `毛利 ${formatCurrency(metrics.minStepRoundTrip.grossProfit.lower)} / ${formatCurrency(
        metrics.minStepRoundTrip.grossProfit.upper,
      )} · 费 ${formatCurrency(metrics.minStepRoundTrip.feeEstimate.lower)} / ${formatCurrency(
        metrics.minStepRoundTrip.feeEstimate.upper,
      )}`,
      source: `当前价 ↔ 下一格，Δ仓位 ${formatSigned(metrics.minStepRoundTrip.exposureUnits)} unit · 数量 ${formatQuantity(
        metrics.minStepRoundTrip.quantity,
      )}`,
    },
    {
      title: '当前价到风险边缘',
      primary: formatRiskEdge(metrics.currentPriceRiskEdge),
      secondary: formatRiskSubline(metrics.currentPriceRiskEdge),
      source: '当前价格出发',
    },
    {
      title: '零仓目标点到风险边缘',
      primary: `${formatCurrency(metrics.zeroTargetRiskEdge.theoreticalLoss)} + 费 ${formatCurrency(
        metrics.zeroTargetRiskEdge.closeFeeEstimate,
      )}`,
      secondary: `零仓目标点 ${formatPrice(metrics.zeroTargetPrice)} → ${formatBoundary(
        metrics.zeroTargetRiskEdge.boundary,
      )} ${formatPrice(metrics.zeroTargetRiskEdge.boundaryPrice)}`,
      source: '由 desired_exposure = 0 推导',
    },
  ];

  return (
    <section className="workbench-panel workbench-panel--metrics" aria-label="关键指标区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">关键指标</p>
          <h2 className="workbench-panel__title">当前调参判断面</h2>
        </div>
        <div className="metric-cards__status">
          <StatusBadge tone={priceStatus.tone}>{priceStatus.badge}</StatusBadge>
          <p className="metric-cards__status-note">{priceStatus.note}</p>
        </div>
      </div>

      <div className="metric-cards metric-cards--compact">
        {cards.map((card) => (
          <article className="metric-card" key={card.title}>
            <p className="metric-card__title">{card.title}</p>
            <p className="metric-card__primary">{card.primary}</p>
            <p className="metric-card__secondary">{card.secondary}</p>
            <p className="metric-card__source">{card.source}</p>
          </article>
        ))}
      </div>

      <InlineNotice title="口径说明">
        当前价格、最小步长、风险边缘与零仓目标点全部来自同一套前端领域试算结果；每步理论净利已扣除开平仓手续费估算。
      </InlineNotice>
    </section>
  );
}

function formatBandStatus(kind: TrackMetrics['bandStatus']['kind']) {
  return kind === 'in_band' ? '带内' : '带外';
}

function formatRiskEdge(edge: TrackMetrics['currentPriceRiskEdge']) {
  if (edge.mode === 'single') {
    return `${formatCurrency(edge.edge.theoreticalLoss)} + 费 ${formatCurrency(edge.edge.closeFeeEstimate)}`;
  }

  return `下 ${formatCurrency(edge.lower.theoreticalLoss)} / 上 ${formatCurrency(edge.upper.theoreticalLoss)}`;
}

function formatRiskSubline(edge: TrackMetrics['currentPriceRiskEdge']) {
  if (edge.mode === 'single') {
    return `${formatBoundary(edge.edge.boundary)} ${formatPrice(edge.edge.boundaryPrice)} · 距离 ${formatSigned(
      edge.edge.priceDistance,
    )}`;
  }

  return `双向观察 · 下 ${formatPrice(edge.lower.boundaryPrice)} / 上 ${formatPrice(edge.upper.boundaryPrice)}`;
}

function formatBoundary(boundary: 'below' | 'above') {
  return boundary === 'below' ? '下边缘' : '上边缘';
}

function formatPrice(value: number) {
  return value.toFixed(2);
}

function formatCurrency(value: number) {
  return value.toFixed(2);
}

function formatSigned(value: number) {
  return value.toFixed(2);
}

function formatQuantity(value: number) {
  const absolute = Math.abs(value);
  if (absolute >= 1) {
    return value.toFixed(4);
  }
  if (absolute >= 0.01) {
    return value.toFixed(5);
  }
  return value.toFixed(6);
}
