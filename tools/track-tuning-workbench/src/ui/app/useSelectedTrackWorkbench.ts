import { useEffect, useMemo, useRef } from 'react';

import {
  TRACK_NUMERIC_FIELD_KEYS,
  type TrackDraft,
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
} from '@/domain/trackDraft';
import { computeTrackMetrics } from '@/domain/trackMetrics';
import { parseFiniteNumber, validateTrackDraft } from '@/domain/trackValidation';
import type { WorkbenchState } from '@/state/workbenchStore';
import type { TrackListItem } from '@/ui/sidebar/TrackList';

export interface SelectedTrackWorkbenchModel {
  selectedDraft: TrackDraft | null;
  selectedValidation: ReturnType<typeof validateTrackDraft> | null;
  selectedVisualSnapshot: TrackDraftParsedSnapshot | null;
  selectedMetrics: ReturnType<typeof computeTrackMetrics> | null;
  issuesByDraftId: Map<string, ReturnType<typeof validateTrackDraft>['issues']>;
  trackItems: TrackListItem[];
  priceStatus: {
    tone: 'accent' | 'warning' | 'danger';
    badge: string;
    note: string;
  };
}

export function useSelectedTrackWorkbench(
  snapshot: WorkbenchState,
): SelectedTrackWorkbenchModel {
  const lastValidSnapshotsRef = useRef(new Map<string, TrackDraftParsedSnapshot>());

  const selectedDraft = useMemo(() => resolveSelectedDraft(snapshot), [snapshot]);
  const selectedValidation = useMemo(
    () => (selectedDraft ? validateTrackDraft(selectedDraft) : null),
    [selectedDraft],
  );

  useEffect(() => {
    if (!selectedDraft || !selectedValidation?.parsed) {
      return;
    }
    lastValidSnapshotsRef.current.set(selectedDraft.draftId, selectedValidation.parsed);
  }, [selectedDraft, selectedValidation]);

  const selectedVisualSnapshot = useMemo(
    () =>
      resolveVisualSnapshot(
        selectedDraft,
        selectedValidation?.parsed,
        lastValidSnapshotsRef.current.get(selectedDraft?.draftId ?? ''),
      ),
    [selectedDraft, selectedValidation],
  );

  const selectedMetrics = useMemo(
    () => (selectedVisualSnapshot ? computeTrackMetrics(selectedVisualSnapshot) : null),
    [selectedVisualSnapshot],
  );

  const issuesByDraftId = useMemo(
    () =>
      new Map(
        snapshot.drafts.map((draft) => [draft.draftId, validateTrackDraft(draft).issues]),
      ),
    [snapshot.drafts],
  );

  const trackItems = useMemo(() => {
    const sourceDraftsById = new Map(
      snapshot.sourceDrafts.map((draft) => [draft.draftId, draft]),
    );

    return snapshot.drafts.map((draft) => ({
      draftId: draft.draftId,
      trackId: draft.additional.trackId,
      symbol: draft.additional.symbol,
      isSelected: selectedDraft?.draftId === draft.draftId,
      isDirty: draftChanged(draft, sourceDraftsById.get(draft.draftId)),
      hasErrors: (issuesByDraftId.get(draft.draftId)?.length ?? 0) > 0,
    }));
  }, [issuesByDraftId, selectedDraft, snapshot.drafts, snapshot.sourceDrafts]);

  const priceStatus = useMemo(
    () =>
      resolvePriceStatus(
        selectedValidation?.issues ?? [],
        selectedDraft,
        selectedVisualSnapshot,
        selectedValidation?.parsed ?? null,
      ),
    [selectedDraft, selectedValidation, selectedVisualSnapshot],
  );

  return {
    selectedDraft,
    selectedValidation,
    selectedVisualSnapshot,
    selectedMetrics,
    issuesByDraftId,
    trackItems,
    priceStatus,
  };
}

function resolveSelectedDraft(snapshot: WorkbenchState) {
  if (snapshot.drafts.length === 0) {
    return null;
  }

  return (
    snapshot.drafts.find((draft) => draft.draftId === snapshot.selectedDraftId)
    ?? snapshot.drafts[0]
  );
}

function draftChanged(current: TrackDraft, source?: TrackDraft) {
  if (!source) {
    return true;
  }
  return JSON.stringify(current) !== JSON.stringify(source);
}

function resolvePriceStatus(
  issues: Array<{ field: string; message: string }>,
  selectedDraft: TrackDraft | null,
  visualSnapshot: TrackDraftParsedSnapshot | null,
  currentParsedSnapshot: TrackDraftParsedSnapshot | null,
) {
  const quoteIssue = issues.find((issue) => issue.field === 'quotePriceInput');
  if (quoteIssue) {
    return {
      tone: 'danger' as const,
      badge: '价格不可用',
      note: quoteIssue.message,
    };
  }

  if (issues.length > 0 && visualSnapshot && !currentParsedSnapshot) {
    return {
      tone: 'warning' as const,
      badge: '沿用最近可用结果',
      note: '当前输入有误，主图和指标先保留最近一次合法试算结果。',
    };
  }

  if (!selectedDraft) {
    return {
      tone: 'warning' as const,
      badge: '等待 Track',
      note: '先从左栏选择一个可编辑的 Track。',
    };
  }

  return {
    tone: 'accent' as const,
    badge: 'Binance 待接通',
    note: '当前用本地输入价格试算；Task 7 接命令后再显示来源和失败原因。',
  };
}

function resolveVisualSnapshot(
  draft: TrackDraft | null,
  currentParsedSnapshot: TrackDraftParsedSnapshot | undefined,
  lastValidSnapshot: TrackDraftParsedSnapshot | undefined,
): TrackDraftParsedSnapshot | null {
  if (currentParsedSnapshot) {
    return currentParsedSnapshot;
  }

  if (lastValidSnapshot) {
    return lastValidSnapshot;
  }

  if (!draft) {
    return null;
  }

  const fallbackNumbers = completeFallbackParsedNumbers(draft.parsedNumbers);
  const quotePrice = parseFiniteNumber(draft.ui.quotePriceInput);

  if (!fallbackNumbers || quotePrice === null) {
    return null;
  }

  return {
    draftId: draft.draftId,
    additional: draft.additional,
    parsedNumbers: fallbackNumbers,
    enums: draft.enums,
    ui: {
      quotePriceInput: draft.ui.quotePriceInput,
      quotePrice,
    },
    attachments: draft.attachments,
  };
}

function completeFallbackParsedNumbers(
  parsedNumbers: Partial<TrackDraftNumericFields>,
): TrackDraftNumericFields | null {
  const complete = {} as TrackDraftNumericFields;

  for (const field of TRACK_NUMERIC_FIELD_KEYS) {
    const value = parsedNumbers[field];
    if (typeof value !== 'number' || !Number.isFinite(value)) {
      return null;
    }
    complete[field] = value;
  }

  return complete;
}
