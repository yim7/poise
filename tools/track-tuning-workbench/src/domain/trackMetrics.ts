import type { TrackDraftParsedSnapshot } from '@/domain/trackDraft';
import {
  bandCenter,
  bandStatus,
  baseQuantityPerUnit,
  desiredExposure,
  sampleTrackCurve,
  solvePriceForTargetExposure,
} from '@/domain/trackCurve';
import {
  type TrackCurrentPriceRiskEdge,
  computeRiskToBandEdge,
  computeCurrentPriceRiskEdge,
  evaluateTrackRisk,
} from '@/domain/trackRisk';

export interface TrackMetrics {
  bandStatus: ReturnType<typeof bandStatus>;
  currentTargetExposure: number;
  approvedTargetExposure: number;
  baseQuantityPerUnit: number;
  oneUnitQuantity: number;
  oneUnitPrice: {
    lower: number;
    upper: number;
  };
  currentPriceRiskEdge: TrackCurrentPriceRiskEdge;
  zeroTargetPrice: number;
  zeroTargetRiskEdge: ReturnType<typeof computeRiskToBandEdge>;
  minRebalancePriceMove: {
    lower: number;
    upper: number;
  };
  curve: ReturnType<typeof sampleTrackCurve>;
}

export function computeTrackMetrics(snapshot: TrackDraftParsedSnapshot): TrackMetrics {
  const currentPrice = snapshot.ui.quotePrice;
  const currentTargetExposure = desiredExposure(currentPrice, snapshot);
  const oneUnitQuantity = baseQuantityPerUnit(snapshot);
  const zeroTargetPrice = resolveZeroTargetPrice(snapshot);
  const riskDecision = evaluateTrackRisk(
    {
      current: snapshot.attachments.currentExposure ?? currentTargetExposure,
      target: currentTargetExposure,
      unitNotional: snapshot.parsedNumbers.notionalPerUnit,
      lossGuard: snapshot.attachments.lossGuard ?? {
        netRealizedPnlToday: 0,
        netRealizedPnlCumulative: 0,
        unrealizedPnl: 0,
      },
    },
    {
      maxNotional: snapshot.parsedNumbers.maxNotional,
      dailyLossLimit: snapshot.parsedNumbers.dailyLossLimit,
      totalLossLimit: snapshot.parsedNumbers.totalLossLimit,
    },
  );

  return {
    bandStatus: bandStatus(currentPrice, snapshot),
    currentTargetExposure,
    approvedTargetExposure: riskDecision.targetExposure,
    baseQuantityPerUnit: oneUnitQuantity,
    oneUnitQuantity,
    oneUnitPrice: {
      lower: resolvePriceOrFallback(snapshot, currentTargetExposure + 1, currentPrice),
      upper: resolvePriceOrFallback(snapshot, currentTargetExposure - 1, currentPrice),
    },
    currentPriceRiskEdge: computeCurrentPriceRiskEdge(snapshot, currentPrice),
    zeroTargetPrice,
    zeroTargetRiskEdge: computeRiskToBandEdge(snapshot, zeroTargetPrice),
    minRebalancePriceMove: {
      lower: Math.abs(
        currentPrice -
          resolvePriceOrFallback(
            snapshot,
            currentTargetExposure + snapshot.parsedNumbers.minRebalanceUnits,
            currentPrice,
          ),
      ),
      upper: Math.abs(
        currentPrice -
          resolvePriceOrFallback(
            snapshot,
            currentTargetExposure - snapshot.parsedNumbers.minRebalanceUnits,
            currentPrice,
          ),
      ),
    },
    curve: sampleTrackCurve(snapshot),
  };
}

function resolvePriceOrFallback(
  snapshot: TrackDraftParsedSnapshot,
  targetExposure: number,
  fallbackPrice: number,
): number {
  return solvePriceForTargetExposure(snapshot, targetExposure) ?? fallbackPrice;
}

function resolveZeroTargetPrice(snapshot: TrackDraftParsedSnapshot): number {
  if (
    snapshot.parsedNumbers.longExposureUnits + snapshot.parsedNumbers.shortExposureUnits <
    Number.EPSILON
  ) {
    return bandCenter(snapshot);
  }

  return solvePriceForTargetExposure(snapshot, 0) ?? bandCenter(snapshot);
}
