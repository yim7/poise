import {
  TRACK_NUMERIC_FIELD_KEYS,
  type TrackDraft,
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
  type RiskAcquisitionParsed,
} from '@/domain/trackDraft';
import { computeTrackMetrics } from '@/domain/trackMetrics';
import { parseFiniteNumber, validateTrackDraft } from '@/domain/trackValidation';
import type {
  RemoteQuoteState,
  WorkbenchState,
} from '@/state/workbenchStore';
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
  const selectedDraft = resolveSelectedDraft(snapshot);
  const selectedValidation = selectedDraft ? validateTrackDraft(selectedDraft) : null;

  const issuesByDraftId = new Map(
    snapshot.drafts.map((draft) => [draft.draftId, validateTrackDraft(draft).issues]),
  );

  const trackItems = buildTrackItems(snapshot, selectedDraft, issuesByDraftId);
  const selectedVisualSnapshot = resolveVisualSnapshot(
    snapshot,
    selectedDraft,
    selectedValidation?.parsed,
  );
  const selectedMetrics = selectedVisualSnapshot
    ? computeTrackMetrics(selectedVisualSnapshot)
    : null;
  const priceStatus = resolvePriceStatus(
    snapshot,
    selectedDraft,
    selectedValidation?.issues ?? [],
    selectedVisualSnapshot,
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

function buildTrackItems(
  snapshot: WorkbenchState,
  selectedDraft: TrackDraft | null,
  issuesByDraftId: Map<string, ReturnType<typeof validateTrackDraft>['issues']>,
) {
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
}

function resolveSelectedDraft(snapshot: WorkbenchState) {
  if (snapshot.drafts.length === 0) {
    return null;
  }

  return snapshot.drafts.find((draft) => draft.draftId === snapshot.selectedDraftId)
    ?? snapshot.drafts[0];
}

function draftChanged(current: TrackDraft, source?: TrackDraft) {
  if (!source) {
    return true;
  }
  return JSON.stringify(current) !== JSON.stringify(source);
}

function resolveVisualSnapshot(
  snapshot: WorkbenchState,
  draft: TrackDraft | null,
  currentParsedSnapshot: TrackDraftParsedSnapshot | undefined,
): TrackDraftParsedSnapshot | null {
  if (currentParsedSnapshot) {
    return currentParsedSnapshot;
  }

  if (!draft) {
    return null;
  }

  if ((draft.attachments.loadIssues?.length ?? 0) > 0) {
    return null;
  }

  const fallbackNumbers = completeFallbackParsedNumbers(draft.parsedNumbers);
  const quotePrice = resolveQuotePrice(snapshot, draft);

  if (!fallbackNumbers || quotePrice === null) {
    return null;
  }

  return {
    draftId: draft.draftId,
    additional: draft.additional,
    parsedNumbers: fallbackNumbers,
    riskAcquisition: fallbackRiskAcquisition(draft),
    enums: draft.enums,
    ui: {
      quotePriceInput: draft.ui.quotePriceInput,
      quotePrice,
    },
    attachments: draft.attachments,
  };
}

function fallbackRiskAcquisition(draft: TrackDraft): RiskAcquisitionParsed {
  return {
    initialRatio: parseFiniteNumber(draft.riskAcquisition.initialRatio) ?? 0.3,
    advantageSteps: parseFiniteNumber(draft.riskAcquisition.advantageSteps) ?? 2,
    minReleaseSteps: parseFiniteNumber(draft.riskAcquisition.minReleaseSteps) ?? 1,
    maxReleaseSteps: parseFiniteNumber(draft.riskAcquisition.maxReleaseSteps) ?? 4,
    catchupRatio: parseFiniteNumber(draft.riskAcquisition.catchupRatio) ?? 0.25,
    staleReleaseMinutes:
      parseFiniteNumber(draft.riskAcquisition.staleReleaseMinutes) ?? 30,
  };
}

function resolveQuotePrice(snapshot: WorkbenchState, draft: TrackDraft): number | null {
  const override = snapshot.temporaryPriceOverrides[draft.draftId];
  if (typeof override === 'number' && Number.isFinite(override)) {
    return override;
  }

  const quoteText = draft.ui.quotePriceInput.trim();
  if (quoteText.length > 0) {
    return parseFiniteNumber(draft.ui.quotePriceInput);
  }

  const remoteQuote = snapshot.remoteQuotes[draft.draftId];
  if (remoteQuote?.status === 'live') {
    return remoteQuote.price;
  }

  return null;
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

function resolvePriceStatus(
  snapshot: WorkbenchState,
  selectedDraft: TrackDraft | null,
  issues: Array<{ field: string; message: string }>,
  visualSnapshot: TrackDraftParsedSnapshot | null,
) {
  if (!selectedDraft) {
    return {
      tone: 'warning' as const,
      badge: '等待 Track',
      note: '先从左栏选择一个可编辑的 Track。',
    };
  }

  const quoteIssue = issues.find((issue) => issue.field === 'quotePriceInput');
  if (quoteIssue) {
    return {
      tone: 'danger' as const,
      badge: '价格不可用',
      note: quoteIssue.message,
    };
  }

  const venueName = exchangeVenueDisplayName(selectedDraft);
  const override = snapshot.temporaryPriceOverrides[selectedDraft.draftId];
  if (typeof override === 'number' && Number.isFinite(override)) {
    return {
      tone: 'accent' as const,
      badge: '临时价格覆盖',
      note: `当前试算优先使用临时输入价格；清空输入框后会恢复 ${venueName} 自动报价。`,
    };
  }

  const quoteText = selectedDraft.ui.quotePriceInput.trim();
  if (quoteText.length > 0) {
    return {
      tone: 'accent' as const,
      badge: '临时价格覆盖',
      note: `当前试算优先使用临时输入价格；清空输入框后会恢复 ${venueName} 自动报价。`,
    };
  }

  const remoteQuote = snapshot.remoteQuotes[selectedDraft.draftId];
  if (remoteQuote?.status === 'live') {
    return {
      tone: 'accent' as const,
      badge: `${venueName} 实时`,
      note: `最新合约报价已接通，更新时间 ${formatRetrievedAt(remoteQuote.retrievedAt)}。`,
    };
  }

  if (remoteQuote?.status === 'error') {
    return describeRemoteQuoteError(remoteQuote, venueName);
  }

  if (remoteQuote?.status === 'loading') {
    return {
      tone: 'warning' as const,
      badge: `等待 ${venueName}`,
      note: `正在刷新 ${remoteQuote.symbol} 的合约报价。`,
    };
  }

  if (issues.length > 0 && visualSnapshot) {
    return {
      tone: 'warning' as const,
      badge: '沿用最近可用结果',
      note: '当前输入有误，主图和指标先保留最近一次可用的试算结果。',
    };
  }

  return {
    tone: 'warning' as const,
    badge: `等待 ${venueName}`,
    note: `当前还没有可用的 ${venueName} 合约报价。`,
  };
}

function describeRemoteQuoteError(
  remoteQuote: Extract<RemoteQuoteState, { status: 'error' }>,
  venueName: string,
) {
  if (remoteQuote.errorKind === 'unsupported_symbol') {
    return {
      tone: 'danger' as const,
      badge: 'symbol 不支持',
      note: remoteQuote.message,
    };
  }

  if (remoteQuote.errorKind === 'rate_limited') {
    return {
      tone: 'warning' as const,
      badge: `${venueName} 限流`,
      note: remoteQuote.message,
    };
  }

  if (remoteQuote.errorKind === 'temporarily_unavailable') {
    return {
      tone: 'warning' as const,
      badge: '暂时不可用',
      note: remoteQuote.message,
    };
  }

  return {
    tone: 'danger' as const,
    badge: '报价失败',
    note: remoteQuote.message,
  };
}

function exchangeVenueDisplayName(draft: TrackDraft) {
  return draft.attachments.exchangeVenue?.trim().toLowerCase() === 'okx'
    ? 'OKX'
    : 'Binance';
}

function formatRetrievedAt(retrievedAt: number) {
  return new Date(retrievedAt).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}
