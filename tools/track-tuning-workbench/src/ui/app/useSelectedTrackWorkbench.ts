import type { TrackDraft } from '@/domain/trackDraft';
import { validateTrackDraft } from '@/domain/trackValidation';
import type { RemoteQuoteState, WorkbenchState } from '@/state/workbenchStore';
import type { TrackListItem } from '@/ui/sidebar/TrackList';

export interface SelectedTrackWorkbenchModel {
  selectedDraft: TrackDraft | null;
  selectedValidation: ReturnType<typeof validateTrackDraft> | null;
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

  return {
    selectedDraft,
    selectedValidation,
    issuesByDraftId,
    trackItems: buildTrackItems(snapshot, selectedDraft, issuesByDraftId),
    priceStatus: resolvePriceStatus(
      snapshot,
      selectedDraft,
      selectedValidation?.issues ?? [],
    ),
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
  return !source || JSON.stringify(current) !== JSON.stringify(source);
}

function resolvePriceStatus(
  snapshot: WorkbenchState,
  selectedDraft: TrackDraft | null,
  issues: Array<{ field: string; message: string }>,
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
    return temporaryOverrideStatus(venueName);
  }
  if (selectedDraft.ui.quotePriceInput.trim().length > 0) {
    return temporaryOverrideStatus(venueName);
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
  return {
    tone: 'warning' as const,
    badge: `等待 ${venueName}`,
    note: `当前还没有可用的 ${venueName} 合约报价。`,
  };
}

function temporaryOverrideStatus(venueName: string) {
  return {
    tone: 'accent' as const,
    badge: '临时价格覆盖',
    note: `临时价格只影响报价栏；Rust 合成 evaluator 不依赖它。清空后恢复 ${venueName} 自动报价。`,
  };
}

function describeRemoteQuoteError(
  remoteQuote: Extract<RemoteQuoteState, { status: 'error' }>,
  venueName: string,
) {
  if (remoteQuote.errorKind === 'unsupported_symbol') {
    return { tone: 'danger' as const, badge: 'symbol 不支持', note: remoteQuote.message };
  }
  if (remoteQuote.errorKind === 'rate_limited') {
    return { tone: 'warning' as const, badge: `${venueName} 限流`, note: remoteQuote.message };
  }
  if (remoteQuote.errorKind === 'temporarily_unavailable') {
    return { tone: 'warning' as const, badge: '暂时不可用', note: remoteQuote.message };
  }
  return { tone: 'danger' as const, badge: '报价失败', note: remoteQuote.message };
}

function exchangeVenueDisplayName(draft: TrackDraft) {
  return draft.attachments.exchangeVenue?.trim().toLowerCase() === 'okx' ? 'OKX' : 'Binance';
}

function formatRetrievedAt(retrievedAt: number) {
  return new Date(retrievedAt).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}
