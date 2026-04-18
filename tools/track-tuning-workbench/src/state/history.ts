export interface HistoryOptions<T> {
  limit?: number;
  equals?: (left: T, right: T) => boolean;
}

export interface HistoryState<T> {
  past: T[];
  present: T;
  future: T[];
}

export interface History<T> {
  readonly state: HistoryState<T>;
  canUndo(): boolean;
  canRedo(): boolean;
  push(next: T): boolean;
  replacePresent(next: T): void;
  undo(): boolean;
  redo(): boolean;
  reset(next: T): void;
}

const DEFAULT_LIMIT = 50;

export function createHistory<T>(
  initialPresent: T,
  options: HistoryOptions<T> = {},
): History<T> {
  const limit = Math.max(1, options.limit ?? DEFAULT_LIMIT);
  const equals = options.equals ?? Object.is;

  const state: HistoryState<T> = {
    past: [],
    present: initialPresent,
    future: [],
  };

  return {
    get state() {
      return state;
    },
    canUndo() {
      return state.past.length > 0;
    },
    canRedo() {
      return state.future.length > 0;
    },
    push(next) {
      if (equals(state.present, next)) {
        return false;
      }

      state.past.push(state.present);
      trimPast(state.past, limit);
      state.present = next;
      state.future = [];
      return true;
    },
    replacePresent(next) {
      state.present = next;
    },
    undo() {
      if (state.past.length === 0) {
        return false;
      }

      state.future.unshift(state.present);
      state.present = state.past.pop() as T;
      return true;
    },
    redo() {
      if (state.future.length === 0) {
        return false;
      }

      state.past.push(state.present);
      trimPast(state.past, limit);
      state.present = state.future.shift() as T;
      return true;
    },
    reset(next) {
      state.past = [];
      state.present = next;
      state.future = [];
    },
  };
}

function trimPast<T>(past: T[], limit: number) {
  while (past.length > limit) {
    past.shift();
  }
}
