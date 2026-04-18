import { describe, expect, it } from 'vitest';

import {
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
  type TrackDraftUiState,
  createTrackDraft,
} from '@/domain/trackDraft';
import { sampleTrackCurve } from '@/domain/trackCurve';
import { computeTrackMetrics, type TrackMetrics } from '@/domain/trackMetrics';
import { buildTrackDraftSnapshot, validateTrackDraft } from '@/domain/trackValidation';

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

function makeDraft(
  overrides: Partial<TrackDraftParsedSnapshot> = {},
  uiOverrides: Partial<TrackDraftUiState> = {},
) {
  const numericFields = makeNumericFields(overrides.parsedNumbers);
  const quotePriceInput = uiOverrides.quotePriceInput ?? overrides.ui?.quotePriceInput ?? '100';

  return createTrackDraft({
    draftId: overrides.draftId ?? 'draft-btc-core',
    raw: {
      trackId: overrides.additional?.trackId ?? 'btc-core',
      symbol: overrides.additional?.symbol ?? 'BTCUSDT',
      lowerPrice: String(numericFields.lowerPrice),
      upperPrice: String(numericFields.upperPrice),
      longExposureUnits: String(numericFields.longExposureUnits),
      shortExposureUnits: String(numericFields.shortExposureUnits),
      notionalPerUnit: String(numericFields.notionalPerUnit),
      maxNotional: String(numericFields.maxNotional),
      minRebalanceUnits: String(numericFields.minRebalanceUnits),
      leverage: String(numericFields.leverage),
      dailyLossLimit: String(numericFields.dailyLossLimit),
      totalLossLimit: String(numericFields.totalLossLimit),
      shapeFamily: overrides.enums?.shapeFamily ?? 'linear',
      outOfBandPolicy: overrides.enums?.outOfBandPolicy ?? 'freeze',
    },
    ui: {
      quotePriceInput,
    },
    parsedNumbers: numericFields,
    enums: overrides.enums,
    additional: overrides.additional,
    attachments: overrides.attachments,
  });
}

function makeSnapshot(
  overrides: Partial<TrackDraftParsedSnapshot> = {},
  uiOverrides: Partial<TrackDraftUiState> = {},
): TrackDraftParsedSnapshot {
  const numericFields = makeNumericFields(overrides.parsedNumbers);
  const quotePriceInput = uiOverrides.quotePriceInput ?? overrides.ui?.quotePriceInput ?? '100';

  return {
    draftId: overrides.draftId ?? 'draft-btc-core',
    additional: overrides.additional ?? {
      trackId: 'btc-core',
      symbol: 'BTCUSDT',
    },
    parsedNumbers: numericFields,
    enums: overrides.enums ?? {
      shapeFamily: 'linear',
      outOfBandPolicy: 'freeze',
    },
    ui: {
      quotePriceInput,
      quotePrice: Number(quotePriceInput),
    },
    attachments: overrides.attachments ?? {},
  };
}

function expectSingleRiskEdge(riskEdge: TrackMetrics['currentPriceRiskEdge']) {
  expect(riskEdge.mode).toBe('single');
  if (riskEdge.mode !== 'single') {
    throw new Error('expected single risk edge');
  }
  return riskEdge.edge;
}

function expectDualRiskEdge(riskEdge: TrackMetrics['currentPriceRiskEdge']) {
  expect(riskEdge.mode).toBe('dual');
  if (riskEdge.mode !== 'dual') {
    throw new Error('expected dual risk edge');
  }
  return riskEdge;
}

describe('track domain fixtures', () => {
  it('builds a parsed snapshot for valid editable drafts', () => {
    const snapshot = buildTrackDraftSnapshot(makeDraft());

    expectClose(snapshot.ui.quotePrice, 100);
    expectClose(snapshot.parsedNumbers.lowerPrice, 90);
    expectClose(snapshot.parsedNumbers.maxNotional, 3000);
  });

  it('surfaces invalid raw numbers even when parsed cache still holds the last valid value', () => {
    const draft = createTrackDraft({
      draftId: 'draft-btc-core',
      raw: {
        trackId: 'btc-core',
        symbol: 'BTCUSDT',
        lowerPrice: 'oops',
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
      },
      parsedNumbers: makeNumericFields(),
    });

    const result = validateTrackDraft(draft);

    expect(result.isValid).toBe(false);
    expect(result.parsed).toBeUndefined();
    expect(result.issues).toContainEqual({
      field: 'lowerPrice',
      message: 'lower_price 必须是有限数字',
    });
  });

  it('rejects strategy configs when long and short capacity are both zero', () => {
    const result = validateTrackDraft(
      makeDraft({
        parsedNumbers: makeNumericFields({
          longExposureUnits: 0,
          shortExposureUnits: 0,
        }),
      }),
    );

    expect(result.isValid).toBe(false);
    expect(result.parsed).toBeUndefined();
    expect(result.issues).toContainEqual({
      field: 'longExposureUnits',
      message: 'long_exposure_units 和 short_exposure_units 不能同时为 0',
    });
  });

  it('rejects invalid risk budget numbers before building a parsed snapshot', () => {
    const result = validateTrackDraft(
      makeDraft({
        parsedNumbers: makeNumericFields({
          maxNotional: 0,
          dailyLossLimit: 0,
          totalLossLimit: 0,
        }),
      }),
    );

    expect(result.isValid).toBe(false);
    expect(result.parsed).toBeUndefined();
    expect(result.issues).toContainEqual({
      field: 'maxNotional',
      message: 'max_notional 必须大于 0',
    });
    expect(result.issues).toContainEqual({
      field: 'dailyLossLimit',
      message: 'daily_loss_limit 必须大于 0',
    });
    expect(result.issues).toContainEqual({
      field: 'totalLossLimit',
      message: 'total_loss_limit 必须大于 0',
    });
  });

  it('locks symmetric linear semantics and derived metrics', () => {
    const snapshot = makeSnapshot();
    const metrics = computeTrackMetrics(snapshot);
    const currentRiskEdge = expectDualRiskEdge(metrics.currentPriceRiskEdge);

    expectClose(metrics.currentTargetExposure, 0);
    expectClose(metrics.baseQuantityPerUnit, 3.75);
    expectClose(metrics.oneUnitPrice.lower, 98.75);
    expectClose(metrics.oneUnitPrice.upper, 101.25);
    expectClose(metrics.oneUnitQuantity, 3.75);

    expectClose(currentRiskEdge.lower.boundaryPrice, 90);
    expectClose(currentRiskEdge.lower.priceDistance, 10);
    expectClose(currentRiskEdge.lower.theoreticalLoss, -150);
    expectClose(currentRiskEdge.lower.closeFeeEstimate, 0);
    expectClose(currentRiskEdge.upper.boundaryPrice, 110);
    expectClose(currentRiskEdge.upper.priceDistance, 10);
    expectClose(currentRiskEdge.upper.theoreticalLoss, -150);
    expectClose(currentRiskEdge.upper.closeFeeEstimate, 0);

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
    const currentRiskEdge = expectDualRiskEdge(metrics.currentPriceRiskEdge);

    expectClose(metrics.currentTargetExposure, 0);
    expectClose(currentRiskEdge.lower.theoreticalLoss, 0);
    expectClose(currentRiskEdge.upper.theoreticalLoss, 0);
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

  it('keeps out-of-band price distance from the raw current price while clamping only theoretical loss', () => {
    const snapshot = makeSnapshot({}, { quotePriceInput: '125' });
    const metrics = computeTrackMetrics(snapshot);
    const currentRiskEdge = expectSingleRiskEdge(metrics.currentPriceRiskEdge);

    expectClose(metrics.currentTargetExposure, -8);
    expect(metrics.bandStatus.kind).toBe('out_of_band');
    if (metrics.bandStatus.kind !== 'out_of_band') {
      throw new Error('expected out_of_band band status');
    }
    expect(metrics.bandStatus.boundary).toBe('above');
    expectClose(currentRiskEdge.boundaryPrice, 110);
    expectClose(currentRiskEdge.priceDistance, 15);
    expectClose(currentRiskEdge.theoreticalLoss, 0);
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

  it('locks asymmetric min step with responsive search away from center', () => {
    const snapshot = makeSnapshot(
      {
        enums: {
          shapeFamily: 'responsive',
          outOfBandPolicy: 'freeze',
        },
      },
      { quotePriceInput: '101' },
    );
    const metrics = computeTrackMetrics(snapshot);

    expectClose(metrics.currentTargetExposure, -0.2009509145, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.lower, 2.2820583964, SEARCH_TOLERANCE);
    expectClose(metrics.minRebalancePriceMove.upper, 1.1833507633, SEARCH_TOLERANCE);
    expect(metrics.minRebalancePriceMove.lower).toBeGreaterThan(
      metrics.minRebalancePriceMove.upper,
    );
    expectClose(metrics.oneUnitPrice.lower, 97.6303883854, SEARCH_TOLERANCE);
    expectClose(metrics.oneUnitPrice.upper, 103.0568353281, SEARCH_TOLERANCE);
    expect(metrics.oneUnitPrice.lower).toBeLessThan(snapshot.ui.quotePrice);
    expect(metrics.oneUnitPrice.upper).toBeGreaterThan(snapshot.ui.quotePrice);
  });

  it('returns dual current-price risk edges when the current target is near zero', () => {
    const snapshot = makeSnapshot(
      {
        enums: {
          shapeFamily: 'responsive',
          outOfBandPolicy: 'freeze',
        },
      },
      { quotePriceInput: '101' },
    );

    const metrics = computeTrackMetrics(snapshot);
    const currentRiskEdge = expectDualRiskEdge(metrics.currentPriceRiskEdge);

    expectClose(metrics.currentTargetExposure, -0.2009509145, SEARCH_TOLERANCE);
    expectClose(currentRiskEdge.lower.boundaryPrice, 90);
    expectClose(currentRiskEdge.lower.priceDistance, 11);
    expectClose(currentRiskEdge.lower.theoreticalLoss, -115.0947823357, SEARCH_TOLERANCE);
    expectClose(currentRiskEdge.upper.boundaryPrice, 110);
    expectClose(currentRiskEdge.upper.priceDistance, 9);
    expectClose(currentRiskEdge.upper.theoreticalLoss, -115.0947823354, SEARCH_TOLERANCE);
  });
});
