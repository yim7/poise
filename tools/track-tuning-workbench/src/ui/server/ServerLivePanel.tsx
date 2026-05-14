import { useEffect, useMemo, useRef, useState } from 'react';

import {
  DEFAULT_POISE_SERVER_BASE_URL,
  createPoiseServerClient,
  normalizeServerBaseUrl,
  type PoiseServerClient,
  type StreamEvent,
  type TrackDetailView,
  type TrackDiagnosticsView,
  type TrackListItemView,
  type TrackLiveView,
} from '@/app/poiseServerClient';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { StatusBadge } from '@/ui/common/StatusBadge';

interface ServerLivePanelProps {
  enabled: boolean;
  client?: PoiseServerClient;
  defaultBaseUrl?: string;
}

type ConnectionState = 'idle' | 'connecting' | 'live' | 'error';

export function ServerLivePanel({
  enabled,
  client,
  defaultBaseUrl = DEFAULT_POISE_SERVER_BASE_URL,
}: ServerLivePanelProps) {
  const resolvedClient = useMemo(() => client ?? createPoiseServerClient(), [client]);
  const [baseUrlInput, setBaseUrlInput] = useState(defaultBaseUrl);
  const [connectedBaseUrl, setConnectedBaseUrl] = useState('');
  const [connectionState, setConnectionState] = useState<ConnectionState>('idle');
  const [notice, setNotice] = useState<string | null>(null);
  const [tracks, setTracks] = useState<TrackListItemView[]>([]);
  const [selectedTrackId, setSelectedTrackId] = useState<string>('');
  const [detailsById, setDetailsById] = useState<Record<string, TrackDetailView>>({});
  const [diagnosticsById, setDiagnosticsById] = useState<Record<string, TrackDiagnosticsView>>({});
  const [liveById, setLiveById] = useState<Record<string, TrackLiveView>>({});
  const disconnectRef = useRef<(() => void) | null>(null);

  useEffect(() => () => {
    disconnectRef.current?.();
  }, []);

  if (!enabled) {
    return (
      <section className="workbench-panel workbench-panel--server-live" aria-label="Server live inspector">
        <div className="workbench-panel__header">
          <div>
            <p className="workbench-panel__eyebrow">Server live</p>
            <h2 className="workbench-panel__title">运行态 Inspector</h2>
          </div>
          <StatusBadge tone="neutral">浏览器预览模式</StatusBadge>
        </div>
        <div className="server-live__body">
          <InlineNotice>server live 只在 Tauri 桌面版启用。</InlineNotice>
        </div>
      </section>
    );
  }

  const selectedTrack = tracks.find((track) => track.id === selectedTrackId) ?? null;
  const selectedDetail = selectedTrackId ? detailsById[selectedTrackId] ?? null : null;
  const selectedDiagnostics = selectedTrackId ? diagnosticsById[selectedTrackId] ?? null : null;
  const selectedLive = selectedTrackId ? liveById[selectedTrackId] ?? null : null;

  const connect = async () => {
    const nextBaseUrl = normalizeServerBaseUrl(baseUrlInput);
    disconnectRef.current?.();
    disconnectRef.current = null;
    setConnectionState('connecting');
    setNotice(null);
    setConnectedBaseUrl(nextBaseUrl);

    try {
      const trackList = await resolvedClient.listTracks(nextBaseUrl);
      setTracks(trackList.items);

      const nextSelectedId = selectedTrackId && trackList.items.some((track) => track.id === selectedTrackId)
        ? selectedTrackId
        : trackList.items[0]?.id ?? '';
      setSelectedTrackId(nextSelectedId);

      if (nextSelectedId) {
        await loadTrackReadModel(nextBaseUrl, nextSelectedId);
      }

      disconnectRef.current = resolvedClient.connectEvents(
        nextBaseUrl,
        applyStreamEvent,
        (error) => {
          setNotice(error.message);
        },
      );
      setConnectionState('live');
    } catch (error) {
      setConnectionState('error');
      setNotice(error instanceof Error ? error.message : String(error));
    }
  };

  const selectTrack = async (trackId: string) => {
    setSelectedTrackId(trackId);
    if (!connectedBaseUrl || detailsById[trackId]) {
      return;
    }

    try {
      await loadTrackReadModel(connectedBaseUrl, trackId);
    } catch (error) {
      setNotice(error instanceof Error ? error.message : String(error));
    }
  };

  const loadTrackReadModel = async (baseUrl: string, trackId: string) => {
    const [detail, diagnostics] = await Promise.all([
      resolvedClient.getTrackDetail(baseUrl, trackId),
      resolvedClient.getTrackDiagnostics(baseUrl, trackId),
    ]);

    setDetailsById((current) => ({
      ...current,
      [trackId]: detail,
    }));
    setDiagnosticsById((current) => ({
      ...current,
      [trackId]: diagnostics,
    }));
  };

  const applyStreamEvent = (event: StreamEvent) => {
    if (event.type === 'track_list_item_changed') {
      setTracks((current) => upsertTrackItem(current, event.item));
      return;
    }

    if (event.type === 'track_detail_changed') {
      setDetailsById((current) => ({
        ...current,
        [event.track_id]: event.detail,
      }));
      return;
    }

    if (event.type === 'track_live_view_changed') {
      setLiveById((current) => ({
        ...current,
        [event.track_id]: event.live,
      }));
    }
  };

  return (
    <section className="workbench-panel workbench-panel--server-live" aria-label="Server live inspector">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">Server live</p>
          <h2 className="workbench-panel__title">运行态 Inspector</h2>
        </div>
        <StatusBadge tone={connectionBadgeTone(connectionState)}>
          {connectionBadge(connectionState)}
        </StatusBadge>
      </div>

      <div className="server-live__body">
        <div className="server-live__connect">
          <label className="field">
            <span className="field__label">Server URL</span>
            <input
              className="field__input"
              value={baseUrlInput}
              onChange={(event) => {
                setBaseUrlInput(event.target.value);
              }}
            />
          </label>
          <button
            className="button button--primary"
            type="button"
            disabled={connectionState === 'connecting'}
            onClick={() => {
              void connect();
            }}
          >
            {connectionState === 'connecting' ? '连接中' : '连接 Server'}
          </button>
        </div>

        {notice ? <InlineNotice tone={connectionState === 'error' ? 'danger' : 'warning'}>{notice}</InlineNotice> : null}

        {tracks.length > 0 ? (
          <div className="server-live__layout">
            <ul className="server-live__tracks" aria-label="Server Track 列表">
              {tracks.map((track) => (
                <li
                  className={`server-live__track${track.id === selectedTrackId ? ' server-live__track--selected' : ''}`}
                  key={track.id}
                >
                  <button
                    className="server-live__track-button"
                    type="button"
                    onClick={() => {
                      void selectTrack(track.id);
                    }}
                  >
                    <span className="server-live__track-id">{track.id}</span>
                    <span className="server-live__track-symbol">{track.instrument.symbol}</span>
                  </button>
                  <StatusBadge tone={statusTone(track.lifecycle.status)}>
                    {track.lifecycle.status}
                  </StatusBadge>
                </li>
              ))}
            </ul>

            <div className="server-live__detail">
              {selectedTrack ? (
                <TrackRuntimeSummary
                  detail={selectedDetail}
                  diagnostics={selectedDiagnostics}
                  live={selectedLive}
                  track={selectedTrack}
                />
              ) : null}
            </div>
          </div>
        ) : (
          <div className="empty-state empty-state--wide">
            <p className="empty-state__title">等待连接 Server</p>
            <p className="empty-state__body">连接后会读取 Track read-model，并订阅 live view 推送。</p>
          </div>
        )}
      </div>
    </section>
  );
}

interface TrackRuntimeSummaryProps {
  track: TrackListItemView;
  detail: TrackDetailView | null;
  diagnostics: TrackDiagnosticsView | null;
  live: TrackLiveView | null;
}

function TrackRuntimeSummary({
  track,
  detail,
  diagnostics,
  live,
}: TrackRuntimeSummaryProps) {
  const strategyPrice = live?.strategy_price ?? detail?.status.strategy_price ?? track.strategy_price;
  const markPrice = live?.mark_price ?? detail?.market.mark_price ?? null;
  const currentExposure = detail?.position.current_exposure ?? track.exposure.current;
  const desiredExposure = live?.desired_exposure ?? detail?.position.desired_exposure ?? track.exposure.target ?? null;
  const riskAcquisition = live
    ? live.risk_acquisition ?? null
    : detail?.execution.risk_acquisition ?? null;
  const inventoryGap = detail?.execution.inventory_gap ?? (
    desiredExposure === null ? null : desiredExposure - currentExposure
  );
  const exposureNote = riskAcquisition
    ? `释放 ${formatExposure(riskAcquisition.risk_release_frontier)} · 曲线 ${formatExposure(riskAcquisition.curve_target)}`
    : `目标 ${formatExposure(desiredExposure)}`;
  const gapValue = riskAcquisition
    ? `backlog ${formatSignedExposure(riskAcquisition.backlog_units)}`
    : `差额 ${formatExposure(inventoryGap)}`;
  const gapNote = riskAcquisition
    ? `next ${formatSignedExposure(riskAcquisition.next_release_units)} → ${formatExposure(riskAcquisition.next_release_target)}`
    : `${detail?.execution.active_binding_count ?? track.execution.active_binding_count} 个 active binding`;
  const firstBinding = detail?.execution.bindings[0] ?? null;
  const latestActivity = [
    ...(detail?.activity ?? []),
    ...(diagnostics?.items ?? []),
  ].slice(-4);

  return (
    <>
      <div className="server-live__summary-grid">
        <RuntimeMetric label="Track" value={track.id} note={track.instrument.symbol} />
        <RuntimeMetric label="价格" value={`策略价 ${formatPrice(strategyPrice)}`} note={`标记价 ${formatPrice(markPrice)}`} />
        <RuntimeMetric label="仓位" value={`当前 ${formatExposure(currentExposure)}`} note={exposureNote} />
        <RuntimeMetric label="差额" value={gapValue} note={gapNote} />
      </div>

      <div className="server-live__section">
        <p className="server-live__section-title">执行绑定</p>
        {firstBinding ? (
          <div className="server-live__binding">
            <div>
              <p className="server-live__binding-title">{firstBinding.label}</p>
              <p className="server-live__muted">{firstBinding.policy} · {firstBinding.status}</p>
            </div>
            <StatusBadge tone="accent">
              {firstBinding.order ? formatOrder(firstBinding.order) : firstBinding.intent}
            </StatusBadge>
          </div>
        ) : (
          <p className="server-live__muted">当前没有 active binding。</p>
        )}
      </div>

      <div className="server-live__section">
        <p className="server-live__section-title">最近活动</p>
        {latestActivity.length > 0 ? (
          <ul className="server-live__activity">
            {latestActivity.map((item) => (
              <li className="server-live__activity-item" key={`${item.ts}-${item.message}`}>
                <StatusBadge tone={activityTone(item.level)}>{item.level}</StatusBadge>
                <span>{item.message}</span>
              </li>
            ))}
          </ul>
        ) : (
          <p className="server-live__muted">暂无活动。</p>
        )}
      </div>
    </>
  );
}

function RuntimeMetric({
  label,
  value,
  note,
}: {
  label: string;
  value: string;
  note: string;
}) {
  return (
    <div className="server-live__metric">
      <p className="server-live__metric-label">{label}</p>
      <p className="server-live__metric-value">{value}</p>
      <p className="server-live__metric-note">{note}</p>
    </div>
  );
}

function upsertTrackItem(items: TrackListItemView[], nextItem: TrackListItemView) {
  const index = items.findIndex((item) => item.id === nextItem.id);
  if (index === -1) {
    return [...items, nextItem];
  }

  return items.map((item) => (item.id === nextItem.id ? nextItem : item));
}

function connectionBadge(state: ConnectionState) {
  if (state === 'connecting') {
    return '连接中';
  }
  if (state === 'live') {
    return 'Live';
  }
  if (state === 'error') {
    return '连接失败';
  }
  return '未连接';
}

function connectionBadgeTone(state: ConnectionState) {
  if (state === 'live') {
    return 'success' as const;
  }
  if (state === 'error') {
    return 'danger' as const;
  }
  if (state === 'connecting') {
    return 'warning' as const;
  }
  return 'neutral' as const;
}

function statusTone(status: TrackListItemView['lifecycle']['status']) {
  if (status === 'active') {
    return 'success' as const;
  }
  if (status === 'paused' || status === 'waiting_market_data') {
    return 'warning' as const;
  }
  if (status === 'terminated') {
    return 'danger' as const;
  }
  return 'accent' as const;
}

function activityTone(level: 'info' | 'warn' | 'error') {
  if (level === 'error') {
    return 'danger' as const;
  }
  if (level === 'warn') {
    return 'warning' as const;
  }
  return 'neutral' as const;
}

function formatPrice(value: number | null | undefined) {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return '--';
  }
  return value.toFixed(2);
}

function formatExposure(value: number | null | undefined) {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return '--';
  }
  return value.toFixed(4);
}

function formatSignedExposure(value: number | null | undefined) {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return '--';
  }
  return `${value >= 0 ? '+' : ''}${formatExposure(value)}`;
}

function formatOrder(order: { side: string; price: number; quantity: number }) {
  return `${order.side} ${formatExposure(order.quantity)} @ ${formatPrice(order.price)}`;
}
