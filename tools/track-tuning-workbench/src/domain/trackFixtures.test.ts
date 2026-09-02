import { describe, expect, it } from 'vitest';

import {
  createTrackDraft,
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
  type TrackDraftUiState,
} from '@/domain/trackDraft';
import {
  buildTrackDraftSnapshot,
  clearResolvedLoadIssues,
  validateTrackDraft,
} from '@/domain/trackValidation';

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
  const quotePriceInput = uiOverrides.quotePriceInput
    ?? overrides.ui?.quotePriceInput
    ?? '100';

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
      bandProtectionPolicy: overrides.enums?.bandProtectionPolicy ?? 'freeze',
    },
    ui: { quotePriceInput },
    riskAcquisition: {
      initialRatio: String(overrides.riskAcquisition?.initialRatio ?? 0.37),
      advantageSteps: String(overrides.riskAcquisition?.advantageSteps ?? 1.25),
      minReleaseSteps: String(overrides.riskAcquisition?.minReleaseSteps ?? 0.75),
      maxReleaseSteps: String(overrides.riskAcquisition?.maxReleaseSteps ?? 3),
      catchupRatio: String(overrides.riskAcquisition?.catchupRatio ?? 0.2),
      staleReleaseMinutes: String(overrides.riskAcquisition?.staleReleaseMinutes ?? 45),
    },
    parsedNumbers: numericFields,
    enums: overrides.enums,
    additional: overrides.additional,
    attachments: overrides.attachments,
  });
}

describe('track draft validation fixtures', () => {
  it('keeps caller-provided risk acquisition config', () => {
    const draft = makeDraft();

    expect(draft.riskAcquisition).toEqual({
      initialRatio: '0.37',
      advantageSteps: '1.25',
      minReleaseSteps: '0.75',
      maxReleaseSteps: '3',
      catchupRatio: '0.2',
      staleReleaseMinutes: '45',
    });
  });

  it('builds a parsed snapshot for valid editable drafts', () => {
    const snapshot = buildTrackDraftSnapshot(makeDraft());

    expect(snapshot.ui.quotePrice).toBe(100);
    expect(snapshot.parsedNumbers.lowerPrice).toBe(90);
    expect(snapshot.parsedNumbers.maxNotional).toBe(3000);
  });

  it('accepts a finite positive capacity smaller than machine epsilon', () => {
    const result = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({
        longExposureUnits: Number.EPSILON / 2,
        shortExposureUnits: 0,
      }),
    }));

    expect(result.isValid).toBe(true);
  });

  it('surfaces invalid raw numbers even when parsed cache holds a valid value', () => {
    const draft = makeDraft();
    draft.rawNumbers.lowerPrice = 'oops';

    const result = validateTrackDraft(draft);

    expect(result.isValid).toBe(false);
    expect(result.parsed).toBeUndefined();
    expect(result.issues).toContainEqual({
      field: 'lowerPrice',
      message: 'lower_price 必须是有限数字',
    });
  });

  it('rejects strategy configs when long and short capacity are both zero', () => {
    const result = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({ longExposureUnits: 0, shortExposureUnits: 0 }),
    }));

    expect(result.isValid).toBe(false);
    expect(result.issues).toContainEqual({
      field: 'longExposureUnits',
      message: 'long_exposure_units 和 short_exposure_units 不能同时为 0',
    });
  });

  it('rejects capacity totals that overflow finite arithmetic', () => {
    const result = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({
        longExposureUnits: Number.MAX_VALUE,
        shortExposureUnits: Number.MAX_VALUE,
      }),
    }));

    expect(result.isValid).toBe(false);
    expect(result.issues).toContainEqual({
      field: 'longExposureUnits',
      message: 'long_exposure_units 和 short_exposure_units 的合计必须是有限数字',
    });
  });

  it('rejects non-positive price bounds', () => {
    const lower = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({ lowerPrice: 0 }),
    }));
    const upper = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({ upperPrice: 0 }),
    }));

    expect(lower.issues).toContainEqual({
      field: 'lowerPrice',
      message: 'lower_price 必须大于 0',
    });
    expect(upper.issues).toContainEqual({
      field: 'upperPrice',
      message: 'upper_price 必须大于 0',
    });
  });

  it('rejects zero min rebalance units and non-positive budgets', () => {
    const result = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({
        minRebalanceUnits: 0,
        maxNotional: 0,
        dailyLossLimit: 0,
        totalLossLimit: 0,
      }),
    }));

    expect(result.issues).toEqual(expect.arrayContaining([
      { field: 'minRebalanceUnits', message: 'min_rebalance_units 必须大于 0' },
      { field: 'maxNotional', message: 'max_notional 必须大于 0' },
      { field: 'dailyLossLimit', message: 'daily_loss_limit 必须大于 0' },
      { field: 'totalLossLimit', message: 'total_loss_limit 必须大于 0' },
    ]));
  });

  it('rejects leverage unless it is a positive integer', () => {
    const fractional = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({ leverage: 3.7 }),
    }));
    const zero = validateTrackDraft(makeDraft({
      parsedNumbers: makeNumericFields({ leverage: 0 }),
    }));

    expect(fractional.issues).toContainEqual({
      field: 'leverage',
      message: 'leverage 必须是正整数',
    });
    expect(zero.issues).toContainEqual({
      field: 'leverage',
      message: 'leverage 必须是正整数',
    });
  });

  it('keeps load issues until the affected field is repaired', () => {
    const draft = makeDraft({}, { quotePriceInput: '' });
    draft.rawNumbers.lowerPrice = '0';
    draft.attachments.loadIssues = [{
      field: 'lowerPrice',
      message: 'track #1: missing numeric field `lower_price`',
    }];

    clearResolvedLoadIssues(draft, 'trackId');
    expect(draft.attachments.loadIssues).toHaveLength(1);

    draft.rawNumbers.lowerPrice = '95';
    clearResolvedLoadIssues(draft, 'lowerPrice');
    expect(draft.attachments.loadIssues).toBeUndefined();
  });
});
