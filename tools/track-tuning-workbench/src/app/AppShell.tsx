import { useEffect, useMemo, useRef, useState } from 'react';

import {
  createTrackDraft,
  TRACK_NUMERIC_FIELD_KEYS,
  type TrackDraft,
  type TrackDraftNumericFields,
  type TrackDraftParsedSnapshot,
} from '@/domain/trackDraft';
import { computeTrackMetrics } from '@/domain/trackMetrics';
import { parseFiniteNumber, validateTrackDraft } from '@/domain/trackValidation';
import { useWorkbenchSnapshot, useWorkbenchStore } from '@/state/workbenchStore';
import { TrackWorkbenchChart } from '@/ui/chart/TrackWorkbenchChart';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { TrackEditor } from '@/ui/editor/TrackEditor';
import { MetricCards } from '@/ui/metrics/MetricCards';
import { FilePanel } from '@/ui/sidebar/FilePanel';
import { TrackList } from '@/ui/sidebar/TrackList';

interface NoticeState {
  tone: 'info' | 'warning';
  message: string;
}

export function AppShell() {
  const store = useWorkbenchStore();
  const snapshot = useWorkbenchSnapshot();
  const [notice, setNotice] = useState<NoticeState | null>(null);
  const lastValidSnapshotsRef = useRef(new Map<string, TrackDraftParsedSnapshot>());

  const selectedDraft = resolveSelectedDraft(snapshot);
  const selectedValidation = selectedDraft ? validateTrackDraft(selectedDraft) : null;
  const selectedVisualSnapshot = useMemo(
    () =>
      resolveVisualSnapshot(
        selectedDraft,
        selectedValidation?.parsed,
        lastValidSnapshotsRef.current.get(selectedDraft?.draftId ?? ''),
      ),
    [selectedDraft, selectedValidation],
  );
  const selectedMetrics = selectedVisualSnapshot ? computeTrackMetrics(selectedVisualSnapshot) : null;

  useEffect(() => {
    if (!selectedDraft || !selectedValidation?.parsed) {
      return;
    }
    lastValidSnapshotsRef.current.set(selectedDraft.draftId, selectedValidation.parsed);
  }, [selectedDraft, selectedValidation]);

  const issuesByDraftId = new Map(
    snapshot.drafts.map((draft) => [draft.draftId, validateTrackDraft(draft).issues]),
  );
  const sourceDraftsById = new Map(snapshot.sourceDrafts.map((draft) => [draft.draftId, draft]));

  const trackItems = snapshot.drafts.map((draft) => ({
    draftId: draft.draftId,
    trackId: draft.additional.trackId,
    symbol: draft.additional.symbol,
    isSelected: selectedDraft?.draftId === draft.draftId,
    isDirty: draftChanged(draft, sourceDraftsById.get(draft.draftId)),
    hasErrors: (issuesByDraftId.get(draft.draftId)?.length ?? 0) > 0,
  }));

  return (
    <div className="app-shell">
      <header className="app-shell__topbar">
        <div>
          <p className="app-shell__kicker">Poise · Track tuning workbench</p>
          <h1 className="app-shell__headline">Track Tuning Workbench</h1>
        </div>
        <p className="app-shell__summary">
          主图是参数判断中心，指标卡和编辑器围绕它组织；当前这轮先把高质量视觉工作台与交互边界落稳。
        </p>
      </header>

      {notice ? (
        <div className="app-shell__notice">
          <InlineNotice tone={notice.tone === 'warning' ? 'warning' : 'info'}>{notice.message}</InlineNotice>
        </div>
      ) : null}

      <main className="app-shell__workspace" aria-label="Track tuning workspace">
        <aside className="app-shell__sidebar">
          <FilePanel
            currentFilePath={snapshot.currentFilePath}
            dirty={snapshot.dirty}
            canUndo={snapshot.canUndo}
            canRedo={snapshot.canRedo}
            hasDrafts={snapshot.drafts.length > 0}
            hasSelection={Boolean(selectedDraft)}
            onChooseFile={() => {
              setNotice({
                tone: 'info',
                message: '选择配置文件会在 Task 7 接通真实命令层。',
              });
            }}
            onUndo={() => {
              store.undo();
              setNotice(null);
            }}
            onRedo={() => {
              store.redo();
              setNotice(null);
            }}
            onCopyCurrent={() => {
              setNotice({
                tone: 'info',
                message: '复制当前 Track 会在 Task 7 接通真实导出命令。',
              });
            }}
            onCopyAll={() => {
              setNotice({
                tone: 'info',
                message: '复制全部 Tracks 会在 Task 7 接通真实导出命令。',
              });
            }}
          />

          <TrackList
            items={trackItems}
            selectedTrackId={selectedDraft?.draftId ?? null}
            onSelect={(draftId) => {
              store.selectDraft(draftId);
              setNotice(null);
            }}
            onAddBlank={() => {
              const blankDraft = createBlankDraft(snapshot.drafts.length);
              store.addDraft(blankDraft);
              setNotice({
                tone: 'info',
                message: `已新增 ${blankDraft.additional.trackId}`,
              });
            }}
            onDuplicateSelected={() => {
              if (!selectedDraft) {
                return;
              }
              const nextDraft = duplicateDraft(selectedDraft);
              store.duplicateDraft(selectedDraft.draftId, nextDraft);
              setNotice({
                tone: 'info',
                message: `已复制 ${selectedDraft.additional.trackId}`,
              });
            }}
            onDelete={(draftId) => {
              const target = snapshot.drafts.find((draft) => draft.draftId === draftId);
              if (!target) {
                return;
              }
              store.deleteDraft(draftId);
              setNotice({
                tone: 'warning',
                message: `已删除 ${target.additional.trackId}，可撤销`,
              });
            }}
          />
        </aside>

        <div className="app-shell__main">
          <MetricCards
            snapshot={selectedVisualSnapshot}
            metrics={selectedMetrics}
            priceStatus={resolvePriceStatus(
              selectedValidation?.issues ?? [],
              selectedDraft,
              selectedVisualSnapshot,
              selectedValidation?.parsed ?? null,
            )}
          />
          <TrackWorkbenchChart
            snapshot={selectedVisualSnapshot}
            metrics={selectedMetrics}
          />
          <TrackEditor
            draft={selectedDraft}
            issues={selectedValidation?.issues ?? []}
            onAdditionalChange={(field, value) => {
              if (!selectedDraft) {
                return;
              }
              store.updateDraft(selectedDraft.draftId, (draft) => {
                draft.additional[field] = value;
              });
            }}
            onNumericChange={(field, value) => {
              if (!selectedDraft) {
                return;
              }
              store.updateDraft(selectedDraft.draftId, (draft) => {
                draft.rawNumbers[field] = value;
              });
            }}
            onEnumChange={(field, value) => {
              if (!selectedDraft) {
                return;
              }
              store.updateDraft(selectedDraft.draftId, (draft) => {
                if (field === 'shapeFamily') {
                  draft.enums.shapeFamily = value as TrackDraft['enums']['shapeFamily'];
                  return;
                }
                draft.enums.outOfBandPolicy = value as TrackDraft['enums']['outOfBandPolicy'];
              });
            }}
            onQuotePriceChange={(value) => {
              if (!selectedDraft) {
                return;
              }
              store.updateDraft(selectedDraft.draftId, (draft) => {
                draft.ui.quotePriceInput = value;
              });
            }}
            onCommit={() => {
              store.commit();
            }}
          />
        </div>
      </main>
    </div>
  );
}

function resolveSelectedDraft(snapshot: ReturnType<typeof useWorkbenchSnapshot>) {
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

function createBlankDraft(index: number) {
  const suffix = index + 1;
  return createTrackDraft({
    draftId: `draft-${Date.now().toString(36)}-${suffix}`,
    raw: {
      trackId: `track-${suffix}`,
      symbol: 'BTCUSDT',
      lowerPrice: '90',
      upperPrice: '110',
      longExposureUnits: '8',
      shortExposureUnits: '8',
      notionalPerUnit: '375',
      maxNotional: '3000',
      minRebalanceUnits: '0.5',
      leverage: '10',
      dailyLossLimit: '120',
      totalLossLimit: '500',
      shapeFamily: 'linear',
      outOfBandPolicy: 'freeze',
    },
    ui: {
      quotePriceInput: '100',
    },
  });
}

function duplicateDraft(source: TrackDraft) {
  const duplicate = structuredClone(source);
  const suffix = Date.now().toString(36);
  duplicate.draftId = `${source.draftId}-${suffix}`;
  duplicate.additional.trackId = `${source.additional.trackId}-copy`;
  return duplicate;
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
