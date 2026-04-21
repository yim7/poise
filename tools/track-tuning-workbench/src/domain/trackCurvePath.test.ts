import { describe, expect, it } from 'vitest';

import { createTrackDraft } from '@/domain/trackDraft';
import { buildTrackDraftSnapshot } from '@/domain/trackValidation';
import { summarizeCurvePath } from '@/domain/trackCurvePath';

function expectClose(actual: number, expected: number, tolerance = 1e-6) {
  expect(Math.abs(actual - expected)).toBeLessThanOrEqual(tolerance);
}

function makeSnapshot(shapeFamily: 'linear' | 'inertial' | 'responsive' = 'linear') {
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
      shapeFamily,
      bandProtectionPolicy: 'freeze',
    },
    enums: {
      shapeFamily,
      bandProtectionPolicy: 'freeze',
    },
    ui: {
      quotePriceInput: '100',
    },
  });

  return buildTrackDraftSnapshot(draft);
}

describe('track curve path summary', () => {
  it('summarizes zero-target build path with weighted average entry, traded notional and theoretical pnl', () => {
    const summary = summarizeCurvePath(makeSnapshot(), 100, 90);

    expectClose(summary.startPrice, 100);
    expectClose(summary.endPrice, 90);
    expectClose(summary.quantity, 30);
    expectClose(summary.signedCost, 2850);
    expectClose(summary.averageEntryPrice ?? 0, 95);
    expectClose(summary.tradedNotional, 2850);
    expectClose(summary.theoreticalPnl, -150);
  });

  it('keeps responsive semantics on the same shared path accumulator', () => {
    const summary = summarizeCurvePath(makeSnapshot('responsive'), 100, 90);

    expectClose(summary.quantity, 30, 1e-4);
    expectClose(summary.averageEntryPrice ?? 0, 93.8461541636, 1e-4);
    expectClose(summary.theoreticalPnl, -115.3846249078, 1e-4);
  });
});
