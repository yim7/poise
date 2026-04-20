import type { TrackDraftParsedSnapshot } from '@/domain/trackDraft';
import {
  baseQuantityPerUnit,
  clampPriceToBand,
  desiredExposure,
} from '@/domain/trackCurve';

export interface TrackCurvePathSummary {
  startPrice: number;
  endPrice: number;
  startQuantity: number;
  endQuantity: number;
  quantity: number;
  signedCost: number;
  tradedNotional: number;
  averageEntryPrice: number | null;
  theoreticalPnl: number;
}

export function summarizeCurvePath(
  snapshot: TrackDraftParsedSnapshot,
  startPrice: number,
  endPrice: number,
  options: {
    segments?: number;
  } = {},
): TrackCurvePathSummary {
  const clampedStartPrice = clampPriceToBand(startPrice, snapshot);
  const clampedEndPrice = clampPriceToBand(endPrice, snapshot);
  const quantityPerUnit = baseQuantityPerUnit(snapshot);
  const startQuantity = desiredExposure(clampedStartPrice, snapshot) * quantityPerUnit;
  const endQuantity = desiredExposure(clampedEndPrice, snapshot) * quantityPerUnit;

  if (Math.abs(clampedStartPrice - clampedEndPrice) <= Number.EPSILON) {
    return {
      startPrice: clampedStartPrice,
      endPrice: clampedEndPrice,
      startQuantity,
      endQuantity,
      quantity: 0,
      signedCost: 0,
      tradedNotional: 0,
      averageEntryPrice: null,
      theoreticalPnl: 0,
    };
  }

  const segments = options.segments ?? 2048;
  const step = (clampedEndPrice - clampedStartPrice) / segments;
  let previousExposure = desiredExposure(clampedStartPrice, snapshot);
  let deltaQuantity = 0;
  let signedCost = 0;
  let tradedNotional = 0;

  for (let index = 0; index < segments; index += 1) {
    const leftPrice = clampedStartPrice + step * index;
    const rightPrice = index === segments - 1 ? clampedEndPrice : leftPrice + step;
    const nextExposure = desiredExposure(rightPrice, snapshot);
    const segmentDeltaQuantity = (nextExposure - previousExposure) * quantityPerUnit;

    if (Math.abs(segmentDeltaQuantity) > Number.EPSILON) {
      const tradePrice = (leftPrice + rightPrice) / 2;
      deltaQuantity += segmentDeltaQuantity;
      signedCost += segmentDeltaQuantity * tradePrice;
      tradedNotional += Math.abs(segmentDeltaQuantity) * tradePrice;
    }

    previousExposure = nextExposure;
  }

  return {
    startPrice: clampedStartPrice,
    endPrice: clampedEndPrice,
    startQuantity,
    endQuantity,
    quantity: deltaQuantity,
    signedCost,
    tradedNotional,
    averageEntryPrice:
      Math.abs(deltaQuantity) <= Number.EPSILON ? null : signedCost / deltaQuantity,
    theoreticalPnl:
      endQuantity * clampedEndPrice - startQuantity * clampedStartPrice - signedCost,
  };
}
