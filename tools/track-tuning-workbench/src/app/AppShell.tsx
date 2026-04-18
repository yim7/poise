import { useEffect, useState } from 'react';

import {
  createSourceSnapshot,
  type WorkbenchBridge,
} from '@/app/workbenchBridge';
import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { parseFiniteNumber } from '@/domain/trackValidation';
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

export interface AppShellProps {
  bridge?: WorkbenchBridge;
}

const QUOTE_REFRESH_INTERVAL_MS = 15_000;

export function AppShell({ bridge }: AppShellProps) {
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
  const resolvedBridge = bridge;
  const selectedDraftId = selectedDraft?.draftId ?? null;
  const selectedSymbol = selectedDraft?.additional.symbol.trim().toUpperCase() ?? '';

  useEffect(() => {
    if (!resolvedBridge || !selectedDraftId) {
      return;
    }

    if (!selectedSymbol) {
      store.clearRemoteQuote(selectedDraftId);
      return;
    }

    let cancelled = false;

    const refreshQuote = async () => {
      store.setRemoteQuote(selectedDraftId, {
        status: 'loading',
        symbol: selectedSymbol,
      });

      try {
        const quote = await resolvedBridge.fetchBinanceQuote(selectedSymbol);
        if (cancelled) {
          return;
        }

        if (quote.price !== null && quote.errorKind === null) {
          const price = Number(quote.price);
          if (Number.isFinite(price)) {
            store.setRemoteQuote(selectedDraftId, {
              status: 'live',
              symbol: selectedSymbol,
              price,
              retrievedAt: quote.retrievedAt,
            });
            return;
          }
        }

        store.setRemoteQuote(selectedDraftId, {
          status: 'error',
          symbol: selectedSymbol,
          errorKind: quote.errorKind ?? 'invalid_response',
          message: quote.errorMessage ?? 'Binance 合约报价不可用',
          retrievedAt: quote.retrievedAt,
        });
      } catch (error) {
        if (cancelled) {
          return;
        }

        store.setRemoteQuote(selectedDraftId, {
          status: 'error',
          symbol: selectedSymbol,
          errorKind: 'network',
          message: error instanceof Error ? error.message : String(error),
          retrievedAt: Date.now(),
        });
      }
    };

    void refreshQuote();
    const timer = window.setInterval(() => {
      void refreshQuote();
    }, QUOTE_REFRESH_INTERVAL_MS);

    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [resolvedBridge, selectedDraftId, selectedSymbol, store]);

  return (
    <div className="app-shell">
      <header className="app-shell__topbar">
        <div>
          <p className="app-shell__kicker">Poise · Track tuning workbench</p>
          <h1 className="app-shell__headline">Track Tuning Workbench</h1>
        </div>
        <p className="app-shell__summary">
          主图负责把价格带、仓位曲线和风险边缘放到同一个判断面里；左侧管理文件和 Track，右侧直接围绕真实试算结果调参。
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
            priceBadge={priceStatus.badge}
            priceBadgeTone={priceStatus.tone}
            priceNote={priceStatus.note}
            onChooseFile={async () => {
              if (!resolvedBridge) {
                return;
              }

              try {
                const configPath = await resolvedBridge.openConfigFile();
                if (!configPath) {
                  return;
                }
                const loadedConfig = await resolvedBridge.loadConfigFile(configPath);
                await store.load(loadedConfig.configPath, createSourceSnapshot(loadedConfig));
                setNotice({
                  tone: 'info',
                  message: `已加载 ${loadedConfig.projectedTracks.length} 条 Track`,
                });
              } catch (error) {
                setNotice({
                  tone: 'warning',
                  message: error instanceof Error ? error.message : String(error),
                });
              }
            }}
            onUndo={() => {
              store.undo();
              setNotice(null);
            }}
            onRedo={() => {
              store.redo();
              setNotice(null);
            }}
            onCopyCurrent={async () => {
              if (!resolvedBridge || !selectedDraft) {
                return;
              }

              try {
                const exported = await resolvedBridge.exportCurrentTrack(selectedDraft);
                await resolvedBridge.copyText(exported);
                setNotice({
                  tone: 'info',
                  message: '当前 Track 已复制到剪贴板',
                });
              } catch (error) {
                setNotice({
                  tone: 'warning',
                  message: error instanceof Error ? error.message : String(error),
                });
              }
            }}
            onCopyAll={async () => {
              if (!resolvedBridge || snapshot.drafts.length === 0) {
                return;
              }

              try {
                const exported = await resolvedBridge.exportAllTracks(snapshot.drafts);
                await resolvedBridge.copyText(exported);
                setNotice({
                  tone: 'info',
                  message: '全部 Tracks 已复制到剪贴板',
                });
              } catch (error) {
                setNotice({
                  tone: 'warning',
                  message: error instanceof Error ? error.message : String(error),
                });
              }
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
              const trimmed = value.trim();
              store.setTemporaryPriceOverride(
                selectedDraft.draftId,
                trimmed.length === 0 ? undefined : parseFiniteNumber(value) ?? undefined,
              );
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
      quotePriceInput: '',
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
