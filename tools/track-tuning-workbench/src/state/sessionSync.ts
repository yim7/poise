import type { TrackDraft } from '@/domain/trackDraft';

export interface WorkbenchSnapshot {
  selectedDraftId: string;
  drafts: TrackDraft[];
  temporaryPriceOverrides: Record<string, number>;
}

export interface SessionPersistence {
  loadDraft(configPath: string): Promise<WorkbenchSnapshot | null>;
  saveDraft(configPath: string, snapshot: WorkbenchSnapshot): Promise<void>;
}

export interface SessionSync {
  loadDraft(configPath: string): Promise<WorkbenchSnapshot | null>;
  scheduleSave(configPath: string, snapshot: WorkbenchSnapshot): void;
  flush(): Promise<void>;
  dispose(): void;
}

export interface SessionSyncOptions {
  debounceMs?: number;
}

export function createSessionSync(
  persistence: SessionPersistence,
  options: SessionSyncOptions = {},
): SessionSync {
  const debounceMs = Math.max(0, options.debounceMs ?? 250);

  let pendingPath: string | null = null;
  let pendingSnapshot: WorkbenchSnapshot | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let inFlight: Promise<void> = Promise.resolve();

  return {
    async loadDraft(configPath) {
      return persistence.loadDraft(configPath);
    },
    scheduleSave(configPath, snapshot) {
      pendingPath = configPath;
      pendingSnapshot = cloneSnapshot(snapshot);

      if (timer) {
        clearTimeout(timer);
      }

      timer = setTimeout(() => {
        void flushPending();
      }, debounceMs);
    },
    async flush() {
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }

      await flushPending();
    },
    dispose() {
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      pendingPath = null;
      pendingSnapshot = null;
    },
  };

  async function flushPending() {
    if (!pendingPath || !pendingSnapshot) {
      return inFlight;
    }

    const path = pendingPath;
    const snapshot = pendingSnapshot;
    pendingPath = null;
    pendingSnapshot = null;

    inFlight = persistence.saveDraft(path, cloneSnapshot(snapshot));
    await inFlight;
    return inFlight;
  }
}

function cloneSnapshot(snapshot: WorkbenchSnapshot): WorkbenchSnapshot {
  return structuredClone(snapshot);
}
