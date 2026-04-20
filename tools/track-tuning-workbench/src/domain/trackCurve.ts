import type {
  TrackDraftParsedSnapshot,
  TrackShapeFamily,
} from '@/domain/trackDraft';

const SHAPE_EXPONENTS: Record<TrackShapeFamily, number> = {
  linear: 1,
  inertial: 0.65,
  responsive: 1.6,
};

export type TrackBandBoundary = 'below' | 'above';

export type TrackBandStatus =
  | {
      kind: 'in_band';
      targetExposure: number;
    }
  | {
      kind: 'out_of_band';
      boundary: TrackBandBoundary;
      policy: TrackDraftParsedSnapshot['enums']['bandProtectionKind'];
      clampedTargetExposure: number;
    };

export interface TrackCurvePoint {
  price: number;
  targetExposure: number;
}

export interface TrackCurveSample {
  points: TrackCurvePoint[];
}

export interface TrackPriceSearchOptions {
  tolerance?: number;
  maxIterations?: number;
}

export function shapeFamilyExponent(shapeFamily: TrackShapeFamily): number {
  return SHAPE_EXPONENTS[shapeFamily];
}

export function bandCenter(snapshot: TrackDraftParsedSnapshot): number {
  const { lowerPrice, upperPrice } = snapshot.parsedNumbers;
  return (lowerPrice + upperPrice) / 2;
}

export function baseQuantityPerUnit(snapshot: TrackDraftParsedSnapshot): number {
  const center = bandCenter(snapshot);
  if (center <= Number.EPSILON) {
    return 0;
  }
  return snapshot.parsedNumbers.notionalPerUnit / center;
}

export function clampPriceToBand(
  price: number,
  snapshot: TrackDraftParsedSnapshot,
): number {
  return Math.min(
    snapshot.parsedNumbers.upperPrice,
    Math.max(snapshot.parsedNumbers.lowerPrice, price),
  );
}

export function desiredExposure(
  price: number,
  snapshot: TrackDraftParsedSnapshot,
): number {
  const config = snapshot.parsedNumbers;
  const halfBand = (config.upperPrice - config.lowerPrice) / 2;
  const position =
    halfBand <= Number.EPSILON
      ? 0
      : clampNumber((price - bandCenter(snapshot)) / halfBand, -1, 1);
  const span = (config.longExposureUnits + config.shortExposureUnits) / 2;
  const bias = (config.longExposureUnits - config.shortExposureUnits) / 2;
  const magnitude = Math.abs(position) ** shapeFamilyExponent(snapshot.enums.shapeFamily);
  const mirroredShape = position >= 0 ? -magnitude : magnitude;

  return bias + span * mirroredShape;
}

export function bandStatus(
  price: number,
  snapshot: TrackDraftParsedSnapshot,
): TrackBandStatus {
  const { lowerPrice, upperPrice } = snapshot.parsedNumbers;
  if (price < lowerPrice - Number.EPSILON) {
    return {
      kind: 'out_of_band',
      boundary: 'below',
      policy: snapshot.enums.bandProtectionKind,
      clampedTargetExposure: desiredExposure(lowerPrice, snapshot),
    };
  }
  if (price > upperPrice + Number.EPSILON) {
    return {
      kind: 'out_of_band',
      boundary: 'above',
      policy: snapshot.enums.bandProtectionKind,
      clampedTargetExposure: desiredExposure(upperPrice, snapshot),
    };
  }

  return {
    kind: 'in_band',
    targetExposure: desiredExposure(price, snapshot),
  };
}

export function sampleTrackCurve(
  snapshot: TrackDraftParsedSnapshot,
  options: { sampleCount?: number } = {},
): TrackCurveSample {
  const sampleCount = Math.max(2, options.sampleCount ?? 129);
  const { lowerPrice, upperPrice } = snapshot.parsedNumbers;
  const step = (upperPrice - lowerPrice) / (sampleCount - 1);
  const points: TrackCurvePoint[] = [];

  for (let index = 0; index < sampleCount; index += 1) {
    const price = index === sampleCount - 1 ? upperPrice : lowerPrice + step * index;
    points.push({
      price,
      targetExposure: desiredExposure(price, snapshot),
    });
  }

  return { points };
}

export function solvePriceForTargetExposure(
  snapshot: TrackDraftParsedSnapshot,
  targetExposure: number,
  options: TrackPriceSearchOptions = {},
): number | null {
  const { lowerPrice, upperPrice } = snapshot.parsedNumbers;
  const lowerExposure = desiredExposure(lowerPrice, snapshot);
  const upperExposure = desiredExposure(upperPrice, snapshot);
  const maxExposure = Math.max(lowerExposure, upperExposure);
  const minExposure = Math.min(lowerExposure, upperExposure);

  if (targetExposure > maxExposure + Number.EPSILON || targetExposure < minExposure - Number.EPSILON) {
    return null;
  }

  if (Math.abs(targetExposure - lowerExposure) <= Number.EPSILON) {
    return lowerPrice;
  }
  if (Math.abs(targetExposure - upperExposure) <= Number.EPSILON) {
    return upperPrice;
  }

  const tolerance = options.tolerance ?? 1e-7;
  const maxIterations = options.maxIterations ?? 80;
  let low = lowerPrice;
  let high = upperPrice;
  let lowValue = desiredExposure(low, snapshot) - targetExposure;

  for (let iteration = 0; iteration < maxIterations; iteration += 1) {
    const middle = (low + high) / 2;
    const middleValue = desiredExposure(middle, snapshot) - targetExposure;

    if (Math.abs(middleValue) <= tolerance || Math.abs(high - low) <= tolerance) {
      return middle;
    }

    if (Math.sign(middleValue) === Math.sign(lowValue)) {
      low = middle;
      lowValue = middleValue;
    } else {
      high = middle;
    }
  }

  return (low + high) / 2;
}

function clampNumber(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}
