export const DEFAULT_POISE_SERVER_BASE_URL = 'http://127.0.0.1:8000';

export interface TrackListResponse {
  items: TrackListItemView[];
}

export interface TrackListItemView {
  id: string;
  instrument: InstrumentView;
  lifecycle: TrackLifecycleView;
  strategy_price: number | null;
  strategy_price_status: StrategyPriceStatusView;
  exposure: ExposureSummaryView;
  execution: ExecutionBadgeView;
  pnl: TrackListPnlView;
}

export interface InstrumentView {
  venue: string;
  symbol: string;
}

export interface TrackLifecycleView {
  status: TrackStatus;
  updated_at: string;
}

export interface ExposureSummaryView {
  current: number;
  target?: number | null;
}

export interface TrackListPnlView {
  pnl_asset: string;
  total_pnl: number;
}

export interface ExecutionBadgeView {
  state: ExecutionStateView;
  execution_status: ExecutionStatusView;
  active_binding_count: number;
}

export interface TrackDetailView {
  identity: TrackIdentityView;
  status: TrackStatusPanelView;
  strategy: TrackStrategyView;
  max_notional: number;
  loss_limits: TrackLossLimitsView;
  market: TrackMarketView;
  position: TrackPositionView;
  pnl: TrackPnlView;
  execution: TrackExecutionView;
  activity: TrackActivityItemView[];
  available_commands: TrackCommandView[];
}

export interface TrackIdentityView {
  id: string;
  instrument: InstrumentView;
}

export interface TrackStatusPanelView {
  lifecycle: TrackLifecycleView;
  strategy_price: number | null;
  strategy_price_status: StrategyPriceStatusView;
}

export interface TrackStrategyView {
  lower_price: number;
  upper_price: number;
  long_exposure_units: number;
  short_exposure_units: number;
  notional_per_unit: number;
  min_rebalance_units: number;
  shape_family: ShapeFamily;
  out_of_band_policy: unknown;
  risk_acquisition?: RiskAcquisitionConfigView;
}

export interface TrackLossLimitsView {
  daily_loss_limit: number;
  total_loss_limit: number;
}

export interface TrackMarketView {
  mark_price: number | null;
  best_bid: number | null;
  best_ask: number | null;
}

export interface TrackLiveView {
  strategy_price: number | null;
  strategy_price_status: StrategyPriceStatusView;
  mark_price: number | null;
  best_bid: number | null;
  best_ask: number | null;
  desired_exposure: number | null;
  risk_acquisition?: RiskAcquisitionView | null;
  price_execution_block_reason: PriceExecutionBlockReasonView | null;
}

export interface TrackPositionView {
  current_exposure: number;
  desired_exposure?: number | null;
}

export interface TrackPnlView {
  pnl_asset: string;
  gross_realized_pnl: number;
  net_realized_pnl: number;
  unrealized_pnl: number;
  total_pnl: number;
  trading_fee_cumulative: number;
  funding_fee_cumulative: number;
}

export interface TrackExecutionView {
  state: ExecutionStateView;
  execution_status: ExecutionStatusView;
  attention_reasons: string[];
  inventory_gap: number;
  active_binding_count: number;
  risk_acquisition?: RiskAcquisitionView | null;
  bindings: ExecutionBindingView[];
}

export interface RiskAcquisitionConfigView {
  initial_ratio: number;
  advantage_steps: number;
  min_release_steps: number;
  max_release_steps: number;
  catchup_ratio: number;
}

export interface RiskAcquisitionView {
  direction: RiskAcquisitionDirectionView;
  curve_target: number;
  allowed_target: number;
  backlog_units: number;
  anchor_price: number;
  anchor_curve_target: number;
  next_advantage_target: number;
  next_advantage_price?: number | null;
  next_release_units: number;
  next_release_target: number;
}

export interface ExecutionBindingView {
  id: string;
  policy: ExecutionBindingPolicyView;
  label: string;
  status: ExecutionBindingStatusView;
  intent: ExecutionBindingIntentView;
  order?: ExecutionBindingOrderView | null;
}

export interface ExecutionBindingOrderView {
  side: Side;
  price: number;
  quantity: number;
}

export interface TrackActivityItemView {
  ts: string;
  message: string;
  level: ActivityLevelView;
}

export interface TrackDiagnosticsView {
  items: TrackDiagnosticItemView[];
}

export interface TrackDiagnosticItemView {
  ts: string;
  message: string;
  level: ActivityLevelView;
}

export interface TrackCommandView {
  command: TrackCommandType;
  enabled: boolean;
  disabled_reason?: string | null;
}

export type StreamEvent =
  | {
      type: 'track_list_item_changed';
      track_id: string;
      item: TrackListItemView;
    }
  | {
      type: 'track_detail_changed';
      track_id: string;
      detail: TrackDetailView;
    }
  | {
      type: 'track_live_view_changed';
      track_id: string;
      live: TrackLiveView;
    }
  | {
      type: 'account_summary_changed';
      summary: AccountSummaryView;
    };

export interface AccountSummaryView {
  equity?: number | null;
  available?: number | null;
  unrealized_pnl?: number | null;
  day_change_pct?: number | null;
  risk_signal: RiskSignalView;
  reason?: string | null;
  day_base_at?: string | null;
  updated_at?: string | null;
}

export type TrackStatus =
  | 'waiting_market_data'
  | 'active'
  | 'frozen'
  | 'flattening'
  | 'manual_flattening'
  | 'terminated'
  | 'paused';

export type StrategyPriceStatusView = 'live' | 'stale';
export type PriceExecutionBlockReasonView = 'missing_execution_quote' | 'mark_book_divergence';
export type ShapeFamily = 'linear' | 'inertial' | 'responsive';
export type ExecutionStateView = 'open' | 'paused' | 'closed';
export type ExecutionStatusView = 'normal' | 'attention_required';
export type ExecutionBindingStatusView = 'submit_pending' | 'working' | 'cancel_pending';
export type ExecutionBindingIntentView = 'increase_inventory' | 'decrease_inventory';
export type ExecutionBindingPolicyView = 'curve_maker' | 'catch_up' | 'manual_override' | 'reduce_only';
export type Side = 'buy' | 'sell';
export type ActivityLevelView = 'info' | 'warn' | 'error';
export type TrackCommandType = 'pause' | 'resume' | 'terminate' | 'flatten';
export type RiskSignalView = 'normal' | 'attention' | 'critical';
export type RiskAcquisitionDirectionView = 'long' | 'short';

export interface PoiseServerClient {
  listTracks(baseUrl: string): Promise<TrackListResponse>;
  getTrackDetail(baseUrl: string, trackId: string): Promise<TrackDetailView>;
  getTrackDiagnostics(baseUrl: string, trackId: string): Promise<TrackDiagnosticsView>;
  connectEvents(
    baseUrl: string,
    onEvent: (event: StreamEvent) => void,
    onError?: (error: Error) => void,
  ): () => void;
}

interface PoiseServerClientDeps {
  fetch?: FetchLike;
  WebSocket?: WebSocketConstructorLike;
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;
interface WebSocketLike {
  addEventListener(type: string, listener: EventListener): void;
  removeEventListener(type: string, listener: EventListener): void;
  close(): void;
}

type WebSocketConstructorLike = new (url: string) => WebSocketLike;

export function createPoiseServerClient(
  deps: PoiseServerClientDeps = {},
): PoiseServerClient {
  return {
    async listTracks(baseUrl) {
      return requestJson<TrackListResponse>(
        resolveFetch(deps.fetch),
        endpoint(baseUrl, '/tracks'),
        '读取 Track 列表',
      );
    },
    async getTrackDetail(baseUrl, trackId) {
      return requestJson<TrackDetailView>(
        resolveFetch(deps.fetch),
        endpoint(baseUrl, `/tracks/${encodeURIComponent(trackId)}`),
        `读取 Track ${trackId}`,
      );
    },
    async getTrackDiagnostics(baseUrl, trackId) {
      return requestJson<TrackDiagnosticsView>(
        resolveFetch(deps.fetch),
        endpoint(baseUrl, `/debug/tracks/${encodeURIComponent(trackId)}/diagnostics`),
        `读取 Track ${trackId} 诊断`,
      );
    },
    connectEvents(baseUrl, onEvent, onError) {
      const Socket = resolveWebSocket(deps.WebSocket);
      const socket = new Socket(serverWebSocketUrl(baseUrl));

      const handleMessage = (event: Event) => {
        try {
          const message = event as MessageEvent;
          onEvent(JSON.parse(String(message.data)) as StreamEvent);
        } catch (error) {
          onError?.(toError(error, 'Poise server WS 事件解析失败'));
        }
      };
      const handleError = () => {
        onError?.(new Error('Poise server WS 连接异常'));
      };

      socket.addEventListener('message', handleMessage);
      socket.addEventListener('error', handleError);

      return () => {
        socket.removeEventListener('message', handleMessage);
        socket.removeEventListener('error', handleError);
        socket.close();
      };
    },
  };
}

export function normalizeServerBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim();
  return (trimmed.length > 0 ? trimmed : DEFAULT_POISE_SERVER_BASE_URL).replace(/\/+$/, '');
}

export function serverWebSocketUrl(baseUrl: string): string {
  const url = new URL(`${normalizeServerBaseUrl(baseUrl)}/ws`);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
}

function endpoint(baseUrl: string, path: string): string {
  return `${normalizeServerBaseUrl(baseUrl)}${path}`;
}

async function requestJson<T>(
  fetchJson: FetchLike,
  url: string,
  label: string,
): Promise<T> {
  const response = await fetchJson(url, {
    headers: {
      accept: 'application/json',
    },
  });

  if (!response.ok) {
    throw new Error(`${label}失败：HTTP ${response.status}`);
  }

  return response.json() as Promise<T>;
}

function resolveFetch(fetchJson: FetchLike | undefined): FetchLike {
  if (fetchJson) {
    return fetchJson;
  }
  if (typeof fetch !== 'undefined') {
    return fetch.bind(globalThis);
  }
  throw new Error('当前环境不支持 fetch，无法连接 Poise server。');
}

function resolveWebSocket(Socket: WebSocketConstructorLike | undefined): WebSocketConstructorLike {
  if (Socket) {
    return Socket;
  }
  if (typeof WebSocket !== 'undefined') {
    return WebSocket;
  }
  throw new Error('当前环境不支持 WebSocket，无法订阅 Poise server live。');
}

function toError(error: unknown, fallback: string) {
  if (error instanceof Error) {
    return error;
  }
  return new Error(`${fallback}：${String(error)}`);
}
