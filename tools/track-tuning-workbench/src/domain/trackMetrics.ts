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
  minStepRoundTrip: {
    exposureUnits: number;
    quantity: number;
    triggerPrice: {
      lower: number;
      upper: number;
    };
    priceMove: {
      lower: number;
      upper: number;
    };
    grossProfit: {
      lower: number;
      upper: number;
    };
    feeRates: {
      open: number;
      close: number;
    };
    feeEstimate: {
      lower: number;
      upper: number;
    };
    netProfit: {
      lower: number;
      upper: number;
    };
  };
  oneUnitPrice: {
    lower: number;
    upper: number;
  };
  currentPriceRiskEdge: TrackCurrentPriceRiskEdge;
  zeroTargetPrice: number;
  zeroTargetRiskEdge: ReturnType<typeof computeRiskToBandEdge>;
  curve: ReturnType<typeof sampleTrackCurve>;
}

export function computeTrackMetrics(snapshot: TrackDraftParsedSnapshot): TrackMetrics {
  const currentPrice = snapshot.ui.quotePrice;
  const currentTargetExposure = desiredExposure(currentPrice, snapshot);
  const oneUnitQuantity = baseQuantityPerUnit(snapshot);
  const stepExposureUnits = snapshot.parsedNumbers.minRebalanceUnits;
  const lowerTriggerPrice = resolvePriceOrFallback(
    snapshot,
    currentTargetExposure + stepExposureUnits,
    currentPrice,
  );
  const upperTriggerPrice = resolvePriceOrFallback(
    snapshot,
    currentTargetExposure - stepExposureUnits,
    currentPrice,
  );
  const lowerStepPriceMove = Math.abs(currentPrice - lowerTriggerPrice);
  const upperStepPriceMove = Math.abs(currentPrice - upperTriggerPrice);
  const stepQuantity = Math.abs(stepExposureUnits * oneUnitQuantity);
  const openFeeRate =
    snapshot.attachments.exchangeRules?.makerFeeRate
    ?? snapshot.attachments.exchangeRules?.takerFeeRate
    ?? 0;
  const closeFeeRate =
    snapshot.attachments.exchangeRules?.takerFeeRate
    ?? snapshot.attachments.exchangeRules?.makerFeeRate
    ?? 0;
  const lowerStepFeeEstimate =
    stepQuantity * (lowerTriggerPrice * openFeeRate + currentPrice * closeFeeRate);
  const upperStepFeeEstimate =
    stepQuantity * (upperTriggerPrice * openFeeRate + currentPrice * closeFeeRate);
  const lowerStepGrossProfit = stepQuantity * lowerStepPriceMove;
  const upperStepGrossProfit = stepQuantity * upperStepPriceMove;
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
    minStepRoundTrip: {
      exposureUnits: stepExposureUnits,
      quantity: stepQuantity,
      triggerPrice: {
        lower: lowerTriggerPrice,
        upper: upperTriggerPrice,
      },
      priceMove: {
        lower: lowerStepPriceMove,
        upper: upperStepPriceMove,
      },
      grossProfit: {
        lower: lowerStepGrossProfit,
        upper: upperStepGrossProfit,
      },
      feeRates: {
        open: openFeeRate,
        close: closeFeeRate,
      },
      feeEstimate: {
        lower: lowerStepFeeEstimate,
        upper: upperStepFeeEstimate,
      },
      netProfit: {
        lower: lowerStepGrossProfit - lowerStepFeeEstimate,
        upper: upperStepGrossProfit - upperStepFeeEstimate,
      },
    },
    oneUnitPrice: {
      lower: resolvePriceOrFallback(snapshot, currentTargetExposure + 1, currentPrice),
      upper: resolvePriceOrFallback(snapshot, currentTargetExposure - 1, currentPrice),
    },
    currentPriceRiskEdge: computeCurrentPriceRiskEdge(snapshot, currentPrice),
    zeroTargetPrice,
    zeroTargetRiskEdge: computeRiskToBandEdge(snapshot, zeroTargetPrice),
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
