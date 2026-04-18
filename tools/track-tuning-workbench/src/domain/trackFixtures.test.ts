import { describe, expect, it } from 'vitest';

import {
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
  type TrackDraftUiState,
  buildTrackDraftSnapshot,
  createTrackDraft,
} from '@/domain/trackDraft';
import { sampleTrackCurve } from '@/domain/trackCurve';
import { computeTrackMetrics } from '@/domain/trackMetrics';

const SEARCH_TOLERANCE = 1e-4;

function expectClose(actual: number, expected: number, tolerance = 1e-6) {
  expect(Math.abs(actual - expected)).toBeLessThanOrEqual(tolerance);
}

function makeNumericFields(
  overrides: Partial<TrackDraftNumericFields> = {},
): TrackDraftNumericFields {
  return {
    lowerPrice: 90,
    upperPrice: 110,
    longExposureUnits: 8,
    shortExposureUnits: 8,
    notionalPerUnit: 375,
    maxNotional: 3000,
    minRebalanceUnits: 0.5,
    leverage: 10,
    dailyLossLimit: 120,
    totalLossLimit: 500,
    ...overrides,
  };
}

function makeSnapshot(
  overrides: Partial<TrackDraftParsedSnapshot> = {},
  uiOverrides: Partial<TrackDraftUiState> = {},
): TrackDraftParsedSnapshot {
  const draft = createTrackDraft({
    draftId: 'draft-btc-core',
    raw: {
      trackId: 'btc-core',
      symbol: 'BTCUSDT',
      lowerPrice: '90',
      upperPrice: '110',
      longExposureUnits: '8',
      shortExposureUnits: '8',
      notionalPerUnit: '375',
      maxNotional: '3000',
      minRebalanceUnits: '0.5',
      leverage: '10',
      dailyLossLimit: '120',
      totalLossLimit: '500',
      shapeFamily: 'linear',
      outOfBandPolicy: 'freeze',
    },
    ui: {
      quotePriceInput: '100',
      ...uiOverrides,
    },
    parsedNumbers: makeNumericFields(),
    ...overrides,
  });

  return buildTrackDraftSnapshot(draft);
}

describe('track domain fixtures', () => {
  it('locks symmetric linear semantics and derived metrics', () => {
    const snapshot = makeSnapshot();
    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.currentTargetExposure, 0);
    expectClose(metrics.baseQuantityPerUnit, 3.75);
    expectClose(metrics.oneUnitPrice.lower, 98.75);
    expectClose(metrics.oneUnitPrice.upper, 101.25);
    expectClose(metrics.oneUnitQuantity, 3.75);

    expectClose(metrics.currentPriceRiskEdge.boundaryPrice, 90);
    expectClose(metrics.currentPriceRiskEdge.priceDistance, 10);
    expectClose(metrics.currentPriceRiskEdge.theoreticalLoss, -150);
    expectClose(metrics.currentPriceRiskEdge.closeFeeEstimate, 0);

    expectClose(metrics.zeroTargetPrice, 100);
    expectClose(metrics.zeroTargetRiskEdge.boundaryPrice, 90);
    expectClose(metrics.zeroTargetRiskEdge.priceDistance, 10);
    expectClose(metrics.zeroTargetRiskEdge.theoreticalLoss, -150);

    expectClose(metrics.minRebalancePriceMove.lower, 0.625);
    expectClose(metrics.minRebalancePriceMove.upper, 0.625);
  });

  it('locks empty linear semantics', () => {
    const snapshot = makeSnapshot({
      parsedNumbers: makeNumericFields({
        longExposureUnits: 0,
        shortExposureUnits: 0,
        maxNotional: 0,
      }),
    });

    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.currentTargetExposure, 0);
    expectClose(metrics.currentPriceRiskEdge.theoreticalLoss, 0);
    expectClose(metrics.zeroTargetRiskEdge.theoreticalLoss, 0);
    expectClose(metrics.oneUnitPrice.lower, 100);
    expectClose(metrics.oneUnitPrice.upper, 100);
  });

  it('locks responsive curve semantics with real formula sampling', () => {
    const snapshot = makeSnapshot({
      enums: {
        shapeFamily: 'responsive',
        outOfBandPolicy: 'freeze',
      },
    });

    const metrics = computeTrackMetrics(snapshot);
    const curve = sampleTrackCurve(snapshot, { sampleCount: 5 });

    expectClose(metrics.currentTargetExposure, 0);
    expectClose(metrics.oneUnitPrice.lower, 97.2737306683, SEARCH_TOLERANCE);
    expectClose(metrics.oneUnitPrice.upper, 102.7262693317, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.lower, 1.767766953, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.upper, 1.767766953, SEARCH_TOLERANCE);

    expect(curve.points).toHaveLength(5);
    expectClose(curve.points[0]!.price, 90);
    expectClose(curve.points[0]!.targetExposure, 8);
    expectClose(curve.points[2]!.price, 100);
    expectClose(curve.points[2]!.targetExposure, 0);
    expectClose(curve.points[4]!.price, 110);
    expectClose(curve.points[4]!.targetExposure, -8);
  });

  it('locks inertial min step search without linear approximation', () => {
    const snapshot = makeSnapshot({
      enums: {
        shapeFamily: 'inertial',
        outOfBandPolicy: 'freeze',
      },
    });

    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.oneUnitPrice.lower, 99.5920275945, SEARCH_TOLERANCE);
    expectClose(metrics.oneUnitPrice.upper, 100.4079724055, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.lower, 0.1404454646, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.upper, 0.1404454646, SEARCH_TOLERANCE);
  });

  it('treats current price outside the band as clamped target but keeps edge distance at zero', () => {
    const snapshot = makeSnapshot({}, { quotePriceInput: '125' });
    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.currentTargetExposure, -8);
    expect(metrics.bandStatus.kind).toBe('out_of_band');
    if (metrics.bandStatus.kind !== 'out_of_band') {
      throw new Error('expected out_of_band band status');
    }
    expect(metrics.bandStatus.boundary).toBe('above');
    expectClose(metrics.currentPriceRiskEdge.boundaryPrice, 110);
    expectClose(metrics.currentPriceRiskEdge.priceDistance, 0);
    expectClose(metrics.currentPriceRiskEdge.theoreticalLoss, 0);
  });

  it('locks zero-target point when desired exposure crosses zero away from center', () => {
    const snapshot = makeSnapshot({
      parsedNumbers: makeNumericFields({
        longExposureUnits: 10,
        shortExposureUnits: 6,
        maxNotional: 3750,
      }),
    });

    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.zeroTargetPrice, 102.5);
    expectClose(metrics.zeroTargetRiskEdge.boundaryPrice, 90);
    expectClose(metrics.zeroTargetRiskEdge.priceDistance, 12.5);
    expectClose(metrics.zeroTargetRiskEdge.theoreticalLoss, -234.375);
  });

  it('locks asymmetric min step when current price is off center', () => {
    const snapshot = makeSnapshot({}, { quotePriceInput: '96' });
    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.currentTargetExposure, 3.2);
    expectClose(metrics.minRebalancePriceMove.lower, 0.625);
    expectClose(metrics.minRebalancePriceMove.upper, 0.625);
    expectClose(metrics.oneUnitPrice.lower, 94.75);
    expectClose(metrics.oneUnitPrice.upper, 97.25);
  });
});
