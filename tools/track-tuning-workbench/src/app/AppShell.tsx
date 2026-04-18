import { useState } from 'react';

import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { useWorkbenchSnapshot, useWorkbenchStore } from '@/state/workbenchStore';
import { TrackWorkbenchChart } from '@/ui/chart/TrackWorkbenchChart';
import { useSelectedTrackWorkbench } from '@/ui/app/useSelectedTrackWorkbench';
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
  const {
    selectedDraft,
    selectedValidation,
    selectedVisualSnapshot,
    selectedMetrics,
    trackItems,
    priceStatus,
  } = useSelectedTrackWorkbench(snapshot);

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
            priceStatus={priceStatus}
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
