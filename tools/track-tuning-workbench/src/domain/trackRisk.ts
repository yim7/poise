import type { TrackDraftParsedSnapshot } from '@/domain/trackDraft';
import {
  baseQuantityPerUnit,
  clampPriceToBand,
  desiredExposure,
} from '@/domain/trackCurve';

export interface TrackCapacityBudget {
  maxNotional: number;
  dailyLossLimit: number;
  totalLossLimit: number;
}

export interface TrackLossGuardSnapshot {
  netRealizedPnlToday: number;
  netRealizedPnlCumulative: number;
  unrealizedPnl: number;
}

export interface TrackExposureIntent {
  current: number;
  target: number;
  unitNotional: number;
  lossGuard: TrackLossGuardSnapshot;
}

export type TrackRiskDecision =
  | {
      kind: 'allow';
      targetExposure: number;
    }
  | {
      kind: 'cap';
      targetExposure: number;
    };

export interface TrackRiskEdge {
  boundary: 'below' | 'above';
  boundaryPrice: number;
  priceDistance: number;
  theoreticalLoss: number;
  closeFeeRate: number;
  closeFeeEstimate: number;
  edgeTargetExposure: number;
  edgeQuantity: number;
}

export type TrackCurrentPriceRiskEdge =
  | {
      mode: 'single';
      edge: TrackRiskEdge;
    }
  | {
      mode: 'dual';
      lower: TrackRiskEdge;
      upper: TrackRiskEdge;
    };

export function validateCapacityBudget(budget: TrackCapacityBudget): string[] {
  const errors: string[] = [];

  if (!Number.isFinite(budget.maxNotional) || budget.maxNotional <= 0) {
    errors.push('max_notional 必须是大于 0 的有限数字');
  }
  if (!Number.isFinite(budget.dailyLossLimit) || budget.dailyLossLimit <= 0) {
    errors.push('daily_loss_limit 必须是大于 0 的有限数字');
  }
  if (!Number.isFinite(budget.totalLossLimit) || budget.totalLossLimit <= 0) {
    errors.push('total_loss_limit 必须是大于 0 的有限数字');
  }

  return errors;
}

export function evaluateTrackRisk(
  intent: TrackExposureIntent,
  budget: TrackCapacityBudget,
): TrackRiskDecision {
  const dailyLossAmount = Math.max(
    -(intent.lossGuard.netRealizedPnlToday + intent.lossGuard.unrealizedPnl),
    0,
  );
  const totalLossAmount = Math.max(
    -(intent.lossGuard.netRealizedPnlCumulative + intent.lossGuard.unrealizedPnl),
    0,
  );

  if (dailyLossAmount >= budget.dailyLossLimit || totalLossAmount >= budget.totalLossLimit) {
    return {
      kind: 'cap',
      targetExposure: 0,
    };
  }

  if (budget.maxNotional > 0 && intent.unitNotional > 0) {
    const maxAbsExposure = budget.maxNotional / intent.unitNotional;
    if (Math.abs(intent.target) > maxAbsExposure) {
      return {
        kind: 'cap',
        targetExposure: Math.sign(intent.target) * maxAbsExposure,
      };
    }
  }

  return {
    kind: 'allow',
    targetExposure: intent.target,
  };
}

export function computeRiskToBandEdge(
  snapshot: TrackDraftParsedSnapshot,
  startPrice: number,
  options: {
    takerFeeRate?: number;
    boundary?: 'below' | 'above';
  } = {},
): TrackRiskEdge {
  const clampedStartPrice = clampPriceToBand(startPrice, snapshot);
  const currentTargetExposure = desiredExposure(clampedStartPrice, snapshot);
  const boundary =
    options.boundary ?? selectAdverseBoundary(snapshot, clampedStartPrice, currentTargetExposure);
  const boundaryPrice =
    boundary === 'below'
      ? snapshot.parsedNumbers.lowerPrice
      : snapshot.parsedNumbers.upperPrice;
  const theoreticalLoss =
    Math.abs(clampedStartPrice - boundaryPrice) <= Number.EPSILON
      ? 0
      : integrateCurvePnl(snapshot, clampedStartPrice, boundaryPrice);
  const closeFeeRate =
    options.takerFeeRate ?? snapshot.attachments.exchangeRules?.takerFeeRate ?? 0;
  const edgeTargetExposure = desiredExposure(boundaryPrice, snapshot);
  const edgeQuantity = edgeTargetExposure * baseQuantityPerUnit(snapshot);

  return {
    boundary,
    boundaryPrice,
    priceDistance: Math.abs(boundaryPrice - startPrice),
    theoreticalLoss,
    closeFeeRate,
    closeFeeEstimate: Math.abs(edgeQuantity) * boundaryPrice * closeFeeRate,
    edgeTargetExposure,
    edgeQuantity,
  };
}

export function computeCurrentPriceRiskEdge(
  snapshot: TrackDraftParsedSnapshot,
  startPrice: number,
): TrackCurrentPriceRiskEdge {
  const clampedStartPrice = clampPriceToBand(startPrice, snapshot);
  const currentTargetExposure = desiredExposure(clampedStartPrice, snapshot);
  const isOutOfBand =
    startPrice < snapshot.parsedNumbers.lowerPrice - Number.EPSILON
    || startPrice > snapshot.parsedNumbers.upperPrice + Number.EPSILON;

  if (!isOutOfBand && Math.abs(currentTargetExposure) <= snapshot.parsedNumbers.minRebalanceUnits) {
    return {
      mode: 'dual',
      lower: computeRiskToBandEdge(snapshot, startPrice, { boundary: 'below' }),
      upper: computeRiskToBandEdge(snapshot, startPrice, { boundary: 'above' }),
    };
  }

  return {
    mode: 'single',
    edge: computeRiskToBandEdge(snapshot, startPrice),
  };
}

export function integrateCurvePnl(
  snapshot: TrackDraftParsedSnapshot,
  startPrice: number,
  endPrice: number,
  segments = 2048,
): number {
  if (Math.abs(startPrice - endPrice) <= Number.EPSILON) {
    return 0;
  }

  const step = (endPrice - startPrice) / segments;
  let integral = 0;

  for (let index = 0; index < segments; index += 1) {
    const left = startPrice + step * index;
    const right = left + step;
    integral += ((desiredExposure(left, snapshot) + desiredExposure(right, snapshot)) / 2) * step;
  }

  return integral * baseQuantityPerUnit(snapshot);
}

function selectAdverseBoundary(
  snapshot: TrackDraftParsedSnapshot,
  startPrice: number,
  currentTargetExposure: number,
): 'below' | 'above' {
  if (currentTargetExposure > Number.EPSILON) {
    return 'below';
  }
  if (currentTargetExposure < -Number.EPSILON) {
    return 'above';
  }

  const lowerLoss = integrateCurvePnl(snapshot, startPrice, snapshot.parsedNumbers.lowerPrice);
  const upperLoss = integrateCurvePnl(snapshot, startPrice, snapshot.parsedNumbers.upperPrice);

  return lowerLoss <= upperLoss ? 'below' : 'above';
}
