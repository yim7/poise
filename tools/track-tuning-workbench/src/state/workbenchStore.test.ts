import { describe, expect, it } from 'vitest';

import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import {
  createBrowserSessionPersistence,
  createSessionSync,
  type BrowserStorageLike,
  type SessionPersistence,
} from '@/state/sessionSync';
import { createWorkbenchStore, type WorkbenchSnapshot } from '@/state/workbenchStore';

interface DraftOverrides {
  additional?: TrackDraft['additional'];
  enums?: TrackDraft['enums'];
  rawNumbers?: Partial<TrackDraft['rawNumbers']>;
  parsedNumbers?: TrackDraft['parsedNumbers'];
  ui?: TrackDraft['ui'];
  attachments?: TrackDraft['attachments'];
}

function makeDraft(
  draftId: string,
  overrides: DraftOverrides = {},
): TrackDraft {
  return createTrackDraft({
    draftId,
    raw: {
      trackId: overrides.additional?.trackId ?? draftId,
      symbol: overrides.additional?.symbol ?? `${draftId.toUpperCase()}USDT`,
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
      outOfBandPolicy: overrides.enums?.outOfBandPolicy ?? 'freeze',
    },
    additional: overrides.additional,
    enums: overrides.enums,
    parsedNumbers: overrides.parsedNumbers,
    ui: overrides.ui,
    attachments: overrides.attachments,
  });
}

function makeSnapshot(overrides: Partial<WorkbenchSnapshot> = {}): WorkbenchSnapshot {
  return {
    selectedDraftId: overrides.selectedDraftId ?? 'draft-a',
    drafts: overrides.drafts ?? [makeDraft('draft-a'), makeDraft('draft-b')],
    temporaryPriceOverrides: overrides.temporaryPriceOverrides ?? {},
  };
}

function makePersistence(initialSnapshot?: WorkbenchSnapshot): SessionPersistence {
  let savedSnapshot: WorkbenchSnapshot | undefined = initialSnapshot;

  return {
    async loadDraft(configPath) {
      void configPath;
      return savedSnapshot ? structuredClone(savedSnapshot) : null;
    },
    async saveDraft(configPath, snapshot) {
      void configPath;
      savedSnapshot = structuredClone(snapshot);
    },
  };
}

function createMockStorage(): BrowserStorageLike {
  const map = new Map<string, string>();

  return {
    getItem(key: string) {
      return map.has(key) ? map.get(key)! : null;
    },
    setItem(key: string, value: string) {
      map.set(key, value);
    },
    removeItem(key: string) {
      map.delete(key);
    },
  };
}

describe('workbench store', () => {
  it('restores a deleted track with its editable fields intact', () => {
    const store = createWorkbenchStore({ initialSnapshot: makeSnapshot() });

    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '91';
      draft.ui.quotePriceInput = '101';
      draft.attachments.currentExposure = 7;
    });
    store.deleteDraft('draft-a');

    expect(store.getState().drafts).toHaveLength(1);
    expect(store.getState().selectedDraftId).toBe('draft-b');

    store.undo();

    const restored = store.getState().drafts.find((draft) => draft.draftId === 'draft-a');
    expect(restored).toBeDefined();
    expect(restored?.rawNumbers.lowerPrice).toBe('91');
    expect(restored?.ui.quotePriceInput).toBe('101');
    expect(restored?.attachments.currentExposure).toBe(7);
    expect(store.getState().selectedDraftId).toBe('draft-a');
  });

  it('records text edits only once when they are committed', () => {
    const store = createWorkbenchStore({ initialSnapshot: makeSnapshot() });

    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '91';
    });
    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '92';
    });
    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '93';
    });

    expect(store.canUndo()).toBe(false);

    store.commit();

    expect(store.canUndo()).toBe(true);
    expect(store.getState().drafts.find((draft) => draft.draftId === 'draft-a')?.rawNumbers.lowerPrice).toBe(
      '93',
    );

    store.undo();

    expect(store.getState().drafts.find((draft) => draft.draftId === 'draft-a')?.rawNumbers.lowerPrice).toBe(
      '90',
    );
  });

  it('records slider-style preview updates as a single history step', () => {
    const store = createWorkbenchStore({ initialSnapshot: makeSnapshot() });

    store.setTemporaryPriceOverride('draft-a', 101);
    store.setTemporaryPriceOverride('draft-a', 102);
    store.setTemporaryPriceOverride('draft-a', 103);

    expect(store.canUndo()).toBe(false);

    store.commit();

    expect(store.canUndo()).toBe(true);
    expect(store.getState().temporaryPriceOverrides['draft-a']).toBe(103);

    store.undo();

    expect(store.getState().temporaryPriceOverrides['draft-a']).toBeUndefined();
  });

  it('keeps dirty tied to the original source snapshot instead of the persisted snapshot', async () => {
    const sourceSnapshot = makeSnapshot();
    const restoredSnapshot = makeSnapshot({
      drafts: [
        makeDraft('draft-a', {
          rawNumbers: {
            lowerPrice: '95',
          },
        }),
        makeDraft('draft-b'),
      ],
    });
    const persistence = makePersistence(restoredSnapshot);
    const store = createWorkbenchStore({
      initialSnapshot: sourceSnapshot,
      sessionSync: createSessionSync(persistence, { debounceMs: 0 }),
    });

    await store.load('config/track.json');

    expect(store.isDirty()).toBe(true);

    await store.flush();

    expect(store.isDirty()).toBe(true);

    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '90';
    });

    expect(store.isDirty()).toBe(false);
  });

  it('caps undo history at 100 committed snapshots', () => {
    const store = createWorkbenchStore({ initialSnapshot: makeSnapshot() });

    for (let index = 0; index < 101; index += 1) {
      store.updateDraft('draft-a', (draft) => {
        draft.rawNumbers.lowerPrice = String(91 + index);
      });
      store.commit();
    }

    let undoCount = 0;
    while (store.canUndo()) {
      store.undo();
      undoCount += 1;
    }

    expect(undoCount).toBe(100);
  });

  it('keeps local draft changes when switching the selected track', () => {
    const store = createWorkbenchStore({ initialSnapshot: makeSnapshot() });

    store.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.upperPrice = '111';
    });
    store.selectDraft('draft-b');
    store.selectDraft('draft-a');

    expect(store.getState().selectedDraftId).toBe('draft-a');
    expect(store.getState().drafts.find((draft) => draft.draftId === 'draft-a')?.rawNumbers.upperPrice).toBe('111');
  });

  it('restores the same draft snapshot after reopening the same path', async () => {
    const persistence = makePersistence();
    const sync = createSessionSync(persistence, { debounceMs: 0 });
    const firstStore = createWorkbenchStore({
      initialSnapshot: makeSnapshot(),
      sessionSync: sync,
    });

    await firstStore.load('config/track.json');
    firstStore.updateDraft('draft-a', (draft) => {
      draft.rawNumbers.lowerPrice = '95';
      draft.ui.quotePriceInput = '123.45';
    });
    firstStore.setTemporaryPriceOverride('draft-a', 104);
    await firstStore.flush();

    const reopenedStore = createWorkbenchStore({
      sessionSync: createSessionSync(persistence, { debounceMs: 0 }),
    });

    await reopenedStore.load('config/track.json');

    expect(reopenedStore.getState().selectedDraftId).toBe('draft-a');
    expect(reopenedStore.getState().drafts.find((draft) => draft.draftId === 'draft-a')?.rawNumbers.lowerPrice).toBe(
      '95',
    );
    expect(reopenedStore.getState().drafts.find((draft) => draft.draftId === 'draft-a')?.ui.quotePriceInput).toBe(
      '123.45',
    );
    expect(reopenedStore.getState().temporaryPriceOverrides['draft-a']).toBe(104);
  });

  it('persists browser drafts through localStorage keyed by config path', async () => {
    const storage = createMockStorage();
    const persistence = createBrowserSessionPersistence(storage);
    const storedSnapshot = makeSnapshot({
      selectedDraftId: 'draft-b',
      drafts: [
        makeDraft('draft-a'),
        makeDraft('draft-b', {
          rawNumbers: {
            lowerPrice: '96',
          },
        }),
      ],
      temporaryPriceOverrides: {
        'draft-b': 109,
      },
    });

    await persistence.saveDraft('config/a.json', storedSnapshot);

    expect(await persistence.loadDraft('config/a.json')).toEqual(storedSnapshot);
    expect(await persistence.loadDraft('config/b.json')).toBeNull();
  });
});
