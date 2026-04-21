import { describe, expect, it } from 'vitest';

import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { useSelectedTrackWorkbench } from '@/ui/app/useSelectedTrackWorkbench';

function makeDraft(draftId: string, overrides: Partial<TrackDraft> = {}): TrackDraft {
  return createTrackDraft({
    draftId,
    raw: {
      trackId: overrides.additional?.trackId ?? draftId,
      symbol: overrides.additional?.symbol ?? 'BTCUSDT',
      lowerPrice: overrides.rawNumbers?.lowerPrice ?? '90',
      upperPrice: overrides.rawNumbers?.upperPrice ?? '110',
      longExposureUnits: overrides.rawNumbers?.longExposureUnits ?? '8',
      shortExposureUnits: overrides.rawNumbers?.shortExposureUnits ?? '8',
      notionalPerUnit: overrides.rawNumbers?.notionalPerUnit ?? '375',
      maxNotional: overrides.rawNumbers?.maxNotional ?? '3000',
      minRebalanceUnits: overrides.rawNumbers?.minRebalanceUnits ?? '0.5',
      leverage: overrides.rawNumbers?.leverage ?? '10',
      dailyLossLimit: overrides.rawNumbers?.dailyLossLimit ?? '120',
      totalLossLimit: overrides.rawNumbers?.totalLossLimit ?? '500',
      shapeFamily: overrides.enums?.shapeFamily ?? 'linear',
      bandProtectionPolicy: overrides.enums?.bandProtectionPolicy ?? 'freeze',
    },
    additional: overrides.additional,
    enums: overrides.enums,
    parsedNumbers: overrides.parsedNumbers,
    ui: overrides.ui,
    attachments: overrides.attachments,
  });
}


describe('useSelectedTrackWorkbench', () => {
  it('uses the live Binance quote when there is no temporary override', () => {
    const draft = makeDraft('draft-a', {
      ui: {
        quotePriceInput: '',
      },
    });

    const model = useSelectedTrackWorkbench({
      selectedDraftId: draft.draftId,
      drafts: [draft],
      sourceDrafts: [draft],
      temporaryPriceOverrides: {},
      remoteQuotes: {
        [draft.draftId]: {
          status: 'live',
          symbol: 'BTCUSDT',
          price: 101.25,
          retrievedAt: 1_713_400_000_000,
        },
      },
      currentFilePath: '/tmp/config.toml',
      dirty: false,
      canUndo: false,
      canRedo: false,
    });

    expect(model.selectedVisualSnapshot?.ui.quotePrice).toBe(101.25);
    expect(model.priceStatus.badge).toBe('Binance 实时');
  });

  it('prefers the temporary price override over the live Binance quote', () => {
    const draft = makeDraft('draft-a', {
      ui: {
        quotePriceInput: '',
      },
    });

    const model = useSelectedTrackWorkbench({
      selectedDraftId: draft.draftId,
      drafts: [draft],
      sourceDrafts: [draft],
      temporaryPriceOverrides: {
        [draft.draftId]: 98.5,
      },
      remoteQuotes: {
        [draft.draftId]: {
          status: 'live',
          symbol: 'BTCUSDT',
          price: 101.25,
          retrievedAt: 1_713_400_000_000,
        },
      },
      currentFilePath: '/tmp/config.toml',
      dirty: true,
      canUndo: true,
      canRedo: false,
    });

    expect(model.selectedVisualSnapshot?.ui.quotePrice).toBe(98.5);
    expect(model.priceStatus.badge).toBe('临时价格覆盖');
  });

  it('keeps a load-issue track visible but not trialable', () => {
    const draft = makeDraft('draft-a', {
      attachments: {
        loadIssues: [
          {
            field: 'lowerPrice',
            message: 'track #1: missing numeric field `lower_price`',
          },
        ],
      },
      ui: {
        quotePriceInput: '',
      },
    });

    const model = useSelectedTrackWorkbench({
      selectedDraftId: draft.draftId,
      drafts: [draft],
      sourceDrafts: [draft],
      temporaryPriceOverrides: {},
      remoteQuotes: {
        [draft.draftId]: {
          status: 'live',
          symbol: 'BTCUSDT',
          price: 101.25,
          retrievedAt: 1_713_400_000_000,
        },
      },
      currentFilePath: '/tmp/config.toml',
      dirty: false,
      canUndo: false,
      canRedo: false,
    });

    expect(model.trackItems[0]?.hasErrors).toBe(true);
    expect(model.selectedVisualSnapshot).toBeNull();
  });
});
