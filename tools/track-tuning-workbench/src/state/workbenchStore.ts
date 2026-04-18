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
import type { TrackDraft } from '@/domain/trackDraft';

export type WorkbenchSnapshot = SessionSnapshot;

export interface WorkbenchState extends WorkbenchSnapshot {
  currentFilePath: string | null;
  sourceDrafts: TrackDraft[];
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
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
  const initialSnapshot = cloneSnapshot(options.initialSnapshot ?? createEmptySnapshot());
  let currentSourceSnapshot = cloneSnapshot(initialSnapshot);

  const committedHistory = createHistory(cloneSnapshot(initialSnapshot), {
    limit: 100,
    equals: snapshotsEqual,
  });

  let draftSession = cloneSnapshot(committedHistory.state.present);
  let persistedSnapshot = cloneSnapshot(committedHistory.state.present);
  let currentFilePath: string | null = null;
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
        currentSourceSnapshot = cloneSnapshot(sourceSnapshot);
      }

      const loaded = await sessionSync.loadDraft(configPath);
      const nextSnapshot = cloneSnapshot(loaded ?? currentSourceSnapshot);

      committedHistory.reset(cloneSnapshot(nextSnapshot));
      draftSession = cloneSnapshot(nextSnapshot);
      persistedSnapshot = cloneSnapshot(nextSnapshot);
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
        return nextDraft;
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
    addDraft(draft) {
      commitCurrentDraft();
      draftSession = {
        ...draftSession,
        drafts: [...draftSession.drafts, cloneDraft(draft)],
        selectedDraftId: draft.draftId,
      };
      commitCurrentDraft();
    },
    duplicateDraft(sourceDraftId, draft) {
      commitCurrentDraft();
      draftSession = {
        ...draftSession,
        drafts: insertDraftAfter(draftSession.drafts, sourceDraftId, cloneDraft(draft)),
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
      return !snapshotsEqual(draftSession, currentSourceSnapshot);
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
      dirty: !snapshotsEqual(draftSession, currentSourceSnapshot),
      canUndo: committedHistory.canUndo(),
      canRedo: committedHistory.canRedo(),
    };
  }
}

function createEmptySnapshot(): WorkbenchSnapshot {
  return {
    selectedDraftId: '',
    drafts: [],
    temporaryPriceOverrides: {},
  };
}

function cloneSnapshot(snapshot: WorkbenchSnapshot): WorkbenchSnapshot {
  return structuredClone(snapshot);
}

function cloneDraft(draft: TrackDraft): TrackDraft {
  return structuredClone(draft);
}

function snapshotsEqual(left: WorkbenchSnapshot, right: WorkbenchSnapshot): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
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

function createMemoryPersistence(): SessionPersistence {
  let snapshot: WorkbenchSnapshot | null = null;

  return {
    async loadDraft() {
      return snapshot ? cloneSnapshot(snapshot) : null;
    },
    async saveDraft(_configPath, nextSnapshot) {
      snapshot = cloneSnapshot(nextSnapshot);
    },
  };
}
