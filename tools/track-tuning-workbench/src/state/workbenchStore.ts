import {
  createContext,
  createElement,
  type PropsWithChildren,
  useContext,
  useSyncExternalStore,
} from 'react';

import { createHistory } from '@/state/history';
import type {
  SessionPersistence,
  SessionSync,
  WorkbenchSnapshot as SessionSnapshot,
} from '@/state/sessionSync';
import { createSessionSync } from '@/state/sessionSync';
import {
  refreshTrackDraftParsedNumbers,
  type TrackDraft,
} from '@/domain/trackDraft';
import { withBinanceFuturesDefaults } from '@/domain/binanceFuturesDefaults';

export type WorkbenchSnapshot = SessionSnapshot;

export type RemoteQuoteErrorKind =
  | 'unsupported_symbol'
  | 'rate_limited'
  | 'temporarily_unavailable'
  | 'timed_out'
  | 'network'
  | 'upstream'
  | 'invalid_response';

export type RemoteQuoteState =
  | {
      status: 'loading';
      symbol: string;
    }
  | {
      status: 'live';
      symbol: string;
      price: number;
      retrievedAt: number;
    }
  | {
      status: 'error';
      symbol: string;
      errorKind: RemoteQuoteErrorKind;
      message: string;
      retrievedAt: number;
    };

export interface WorkbenchState extends WorkbenchSnapshot {
  currentFilePath: string | null;
  sourceDrafts: TrackDraft[];
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
  remoteQuotes: Record<string, RemoteQuoteState>;
}

export interface WorkbenchStoreOptions {
  initialSnapshot?: WorkbenchSnapshot;
  sessionSync?: SessionSync;
}

export interface WorkbenchStore {
  getState(): WorkbenchState;
  subscribe(listener: () => void): () => void;
  load(configPath: string, sourceSnapshot?: WorkbenchSnapshot): Promise<void>;
  flush(): Promise<void>;
  selectDraft(draftId: string): void;
  updateDraft(draftId: string, updater: (draft: TrackDraft) => void): void;
  setTemporaryPriceOverride(draftId: string, price: number | undefined): void;
  setRemoteQuote(draftId: string, quote: RemoteQuoteState): void;
  clearRemoteQuote(draftId: string): void;
  markTrackExported(draftId: string): void;
  markAllExported(): void;
  addDraft(draft: TrackDraft): void;
  duplicateDraft(sourceDraftId: string, draft: TrackDraft): void;
  deleteDraft(draftId: string): void;
  commit(): void;
  undo(): void;
  redo(): void;
  canUndo(): boolean;
  canRedo(): boolean;
  isDirty(): boolean;
}

const WorkbenchStoreContext = createContext<WorkbenchStore | null>(null);

export function WorkbenchStoreProvider({
  store,
  children,
}: PropsWithChildren<{ store: WorkbenchStore }>) {
  return createElement(WorkbenchStoreContext.Provider, { value: store }, children);
}

export function useWorkbenchStore(): WorkbenchStore {
  const store = useContext(WorkbenchStoreContext);
  if (!store) {
    throw new Error('WorkbenchStoreProvider is required');
  }
  return store;
}

export function useWorkbenchSnapshot(): WorkbenchState {
  const store = useWorkbenchStore();
  return useSyncExternalStore(store.subscribe, () => store.getState(), () => store.getState());
}

export function createWorkbenchStore(options: WorkbenchStoreOptions = {}): WorkbenchStore {
  const sessionSync = options.sessionSync ?? createSessionSync(createMemoryPersistence());
  const initialSnapshot = normalizeSnapshot(options.initialSnapshot ?? createEmptySnapshot());
  let currentSourceSnapshot = cloneSnapshot(initialSnapshot);

  const committedHistory = createHistory(cloneSnapshot(initialSnapshot), {
    limit: 100,
    equals: snapshotsEqual,
  });

  let draftSession = cloneSnapshot(committedHistory.state.present);
  let persistedSnapshot = cloneSnapshot(committedHistory.state.present);
  let currentFilePath: string | null = null;
  let remoteQuotes: Record<string, RemoteQuoteState> = {};
  const listeners = new Set<() => void>();
  let cachedState = createStateSnapshot();

  return {
    getState() {
      return cachedState;
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    async load(configPath, sourceSnapshot) {
      currentFilePath = configPath;
      if (sourceSnapshot) {
        currentSourceSnapshot = normalizeSnapshot(sourceSnapshot);
      }

      const loaded = await sessionSync.loadDraft(configPath);
      const nextSnapshot = normalizeSnapshot(loaded ?? currentSourceSnapshot);

      committedHistory.reset(cloneSnapshot(nextSnapshot));
      draftSession = cloneSnapshot(nextSnapshot);
      persistedSnapshot = cloneSnapshot(nextSnapshot);
      remoteQuotes = {};
      emit();
    },
    async flush() {
      if (!currentFilePath) {
        persistedSnapshot = cloneSnapshot(draftSession);
        emit();
        return;
      }

      if (snapshotsEqual(draftSession, persistedSnapshot)) {
        return;
      }

      sessionSync.scheduleSave(currentFilePath, cloneSnapshot(draftSession));
      await sessionSync.flush();
      persistedSnapshot = cloneSnapshot(draftSession);
      emit();
    },
    selectDraft(draftId) {
      if (draftSession.selectedDraftId === draftId) {
        return;
      }

      if (!hasDraft(draftSession, draftId)) {
        return;
      }

      draftSession = {
        ...draftSession,
        selectedDraftId: draftId,
      };
      scheduleSave();
      emit();
    },
    updateDraft(draftId, updater) {
      let touched = false;
      const nextDrafts = draftSession.drafts.map((draft) => {
        if (draft.draftId !== draftId) {
          return draft;
        }
        touched = true;
        const nextDraft = cloneDraft(draft);
        updater(nextDraft);
        return normalizeDraft(nextDraft);
      });

      if (!touched) {
        return;
      }

      draftSession = {
        ...draftSession,
        drafts: nextDrafts,
      };
      scheduleSave();
      emit();
    },
    setTemporaryPriceOverride(draftId, price) {
      const currentPrice = draftSession.temporaryPriceOverrides[draftId];
      if (currentPrice === price) {
        return;
      }

      const nextOverrides = {
        ...draftSession.temporaryPriceOverrides,
      };

      if (price === undefined) {
        delete nextOverrides[draftId];
      } else {
        nextOverrides[draftId] = price;
      }

      draftSession = {
        ...draftSession,
        temporaryPriceOverrides: nextOverrides,
      };
      scheduleSave();
      emit();
    },
    setRemoteQuote(draftId, quote) {
      remoteQuotes = {
        ...remoteQuotes,
        [draftId]: structuredClone(quote),
      };
      emit();
    },
    clearRemoteQuote(draftId) {
      if (!(draftId in remoteQuotes)) {
        return;
      }

      const nextQuotes = { ...remoteQuotes };
      delete nextQuotes[draftId];
      remoteQuotes = nextQuotes;
      emit();
    },
    markTrackExported(draftId) {
      const currentDraft = draftSession.drafts.find((draft) => draft.draftId === draftId);
      if (!currentDraft) {
        return;
      }

      draftSession = {
        ...draftSession,
        exportedDrafts: replaceExportedDraft(
          draftSession.exportedDrafts ?? [],
          toExportBaselineDraft(currentDraft),
        ),
      };
      scheduleSave();
      emit();
    },
    markAllExported() {
      draftSession = {
        ...draftSession,
        exportedDrafts: draftSession.drafts.map(toExportBaselineDraft),
      };
      scheduleSave();
      emit();
    },
    addDraft(draft) {
      commitCurrentDraft();
      draftSession = {
        ...draftSession,
        drafts: [...draftSession.drafts, normalizeDraft(draft)],
        selectedDraftId: draft.draftId,
      };
      commitCurrentDraft();
    },
    duplicateDraft(sourceDraftId, draft) {
      commitCurrentDraft();
      draftSession = {
        ...draftSession,
        drafts: insertDraftAfter(draftSession.drafts, sourceDraftId, normalizeDraft(draft)),
        selectedDraftId: draft.draftId,
      };
      commitCurrentDraft();
    },
    deleteDraft(draftId) {
      if (!hasDraft(draftSession, draftId)) {
        return;
      }

      commitCurrentDraft();
      const nextDrafts = draftSession.drafts.filter((draft) => draft.draftId !== draftId);
      const nextSelectedDraftId = resolveNextSelectedDraftId(
        draftSession.selectedDraftId,
        draftId,
        nextDrafts,
      );

      draftSession = {
        ...draftSession,
        drafts: nextDrafts,
        selectedDraftId: nextSelectedDraftId,
      };
      commitCurrentDraft();
    },
    commit() {
      commitCurrentDraft();
    },
    undo() {
      if (!committedHistory.undo()) {
        return;
      }

      draftSession = cloneSnapshot(committedHistory.state.present);
      scheduleSave();
      emit();
    },
    redo() {
      if (!committedHistory.redo()) {
        return;
      }

      draftSession = cloneSnapshot(committedHistory.state.present);
      scheduleSave();
      emit();
    },
    canUndo() {
      return committedHistory.canUndo();
    },
    canRedo() {
      return committedHistory.canRedo();
    },
    isDirty() {
      return !exportedDraftsEqual(draftSession.drafts, draftSession.exportedDrafts ?? []);
    },
  };

  function commitCurrentDraft() {
    const nextCommitted = cloneSnapshot(draftSession);
    committedHistory.push(nextCommitted);
    committedHistory.replacePresent(cloneSnapshot(nextCommitted));
    scheduleSave();
    emit();
  }

  function scheduleSave() {
    if (!currentFilePath || snapshotsEqual(draftSession, persistedSnapshot)) {
      return;
    }

    sessionSync.scheduleSave(currentFilePath, cloneSnapshot(draftSession));
  }

  function emit() {
    cachedState = createStateSnapshot();
    listeners.forEach((listener) => {
      listener();
    });
  }

  function createStateSnapshot(): WorkbenchState {
    return {
      ...cloneSnapshot(draftSession),
      currentFilePath,
      sourceDrafts: cloneSnapshot(currentSourceSnapshot).drafts,
      dirty: !exportedDraftsEqual(draftSession.drafts, draftSession.exportedDrafts ?? []),
      canUndo: committedHistory.canUndo(),
      canRedo: committedHistory.canRedo(),
      remoteQuotes: structuredClone(remoteQuotes),
    };
  }
}

function createEmptySnapshot(): WorkbenchSnapshot {
  return {
    selectedDraftId: '',
    drafts: [],
    temporaryPriceOverrides: {},
    exportedDrafts: [],
  };
}

function cloneSnapshot(snapshot: WorkbenchSnapshot): WorkbenchSnapshot {
  return structuredClone(snapshot);
}

function cloneDraft(draft: TrackDraft): TrackDraft {
  return structuredClone(draft);
}

function normalizeSnapshot(snapshot: WorkbenchSnapshot): WorkbenchSnapshot {
  const normalizedDrafts = snapshot.drafts.map((draft) => normalizeDraft(draft));
  return {
    selectedDraftId: snapshot.selectedDraftId,
    drafts: normalizedDrafts,
    temporaryPriceOverrides: {
      ...snapshot.temporaryPriceOverrides,
    },
    exportedDrafts:
      snapshot.exportedDrafts?.map((draft) => normalizeDraft(draft))
      ?? normalizedDrafts.map(toExportBaselineDraft),
  };
}

function normalizeDraft(draft: TrackDraft): TrackDraft {
  const nextDraft = withBinanceFuturesDefaults(cloneDraft(draft));
  refreshTrackDraftParsedNumbers(nextDraft);
  return nextDraft;
}

function snapshotsEqual(left: WorkbenchSnapshot, right: WorkbenchSnapshot): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

function exportedDraftsEqual(left: TrackDraft[], right: TrackDraft[]) {
  return JSON.stringify(left.map(toExportBaselineDraft))
    === JSON.stringify(right.map(toExportBaselineDraft));
}

function hasDraft(snapshot: WorkbenchSnapshot, draftId: string): boolean {
  return snapshot.drafts.some((draft) => draft.draftId === draftId);
}

function resolveNextSelectedDraftId(
  currentSelectedDraftId: string,
  deletedDraftId: string,
  nextDrafts: TrackDraft[],
): string {
  if (currentSelectedDraftId !== deletedDraftId) {
    return currentSelectedDraftId;
  }

  return nextDrafts[0]?.draftId ?? '';
}

function insertDraftAfter(drafts: TrackDraft[], sourceDraftId: string, draft: TrackDraft): TrackDraft[] {
  const sourceIndex = drafts.findIndex((item) => item.draftId === sourceDraftId);
  if (sourceIndex < 0) {
    return [...drafts, draft];
  }

  const nextDrafts = drafts.slice();
  nextDrafts.splice(sourceIndex + 1, 0, draft);
  return nextDrafts;
}

function replaceExportedDraft(exportedDrafts: TrackDraft[], nextDraft: TrackDraft) {
  const nextExportedDrafts = exportedDrafts.map((draft) =>
    draft.draftId === nextDraft.draftId ? nextDraft : draft);

  if (nextExportedDrafts.some((draft) => draft.draftId === nextDraft.draftId)) {
    return nextExportedDrafts;
  }

  return [...nextExportedDrafts, nextDraft];
}

function toExportBaselineDraft(draft: TrackDraft): TrackDraft {
  return {
    draftId: draft.draftId,
    additional: structuredClone(draft.additional),
    rawNumbers: structuredClone(draft.rawNumbers),
    parsedNumbers: {},
    enums: structuredClone(draft.enums),
    ui: {
      quotePriceInput: '',
    },
    attachments: {},
  };
}

function createMemoryPersistence(): SessionPersistence {
  let snapshot: WorkbenchSnapshot | null = null;

  return {
    async loadDraft() {
      return snapshot ? cloneSnapshot(snapshot) : null;
    },
    async saveDraft(_configPath, nextSnapshot) {
      snapshot = normalizeSnapshot(nextSnapshot);
    },
  };
}
