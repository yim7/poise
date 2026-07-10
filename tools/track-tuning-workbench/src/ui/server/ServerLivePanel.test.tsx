import '@testing-library/jest-dom/vitest';

import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type {
  InventoryDecisionView,
  PoiseServerClient,
  StreamEvent,
  TrackDetailView,
  TrackDiagnosticsView,
  TrackListItemView,
} from '@/app/poiseServerClient';
import { ServerLivePanel } from '@/ui/server/ServerLivePanel';

describe('ServerLivePanel', () => {
  it('keeps server live unavailable in browser preview mode', () => {
    const { client } = createFakeServerClient();

    render(<ServerLivePanel enabled={false} client={client} />);

    expect(screen.getByRole('region', { name: 'Server live inspector' })).toBeInTheDocument();
    expect(screen.getByText('浏览器预览模式')).toBeInTheDocument();
    expect(screen.getByText('server live 只在 Tauri 桌面版启用。')).toBeInTheDocument();
    expect(client.listTracks).not.toHaveBeenCalled();
  });

  it('loads server read-model list, detail, and diagnostics on connect', async () => {
    const { client } = createFakeServerClient();

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(client.listTracks).toHaveBeenCalledWith('http://127.0.0.1:8000');
    expect(client.getTrackDetail).toHaveBeenCalledWith('http://127.0.0.1:8000', 'btc-core');
    expect(client.getTrackDiagnostics).toHaveBeenCalledWith('http://127.0.0.1:8000', 'btc-core');
    expect(client.connectEvents).toHaveBeenCalledWith(
      'http://127.0.0.1:8000',
      expect.any(Function),
      expect.any(Function),
    );
    expect(within(panel).getAllByText('btc-core')).toHaveLength(2);
    expect(within(panel).getAllByText('BTCUSDT')).toHaveLength(2);
    expect(within(panel).getByText('策略价 100.50')).toBeInTheDocument();
    expect(within(panel).getByText('标记价 101.25')).toBeInTheDocument();
    expect(within(panel).getByText('当前 1.5000')).toBeInTheDocument();
    expect(within(panel).getByText('revision 7')).toBeInTheDocument();
    expect(within(panel).getByText('waiting_risk_release')).toBeInTheDocument();
    expect(within(panel).getByText('策略 8.0000')).toBeInTheDocument();
    expect(within(panel).getByText('风控 4.0000')).toBeInTheDocument();
    expect(within(panel).getByText('前沿 1.5000')).toBeInTheDocument();
    expect(within(panel).getByText('执行 1.2500')).toBeInTheDocument();
    expect(within(panel).getByText('差额 -0.2500')).toBeInTheDocument();
    expect(within(panel).getByText('buy 0.2500 @ 100.20')).toBeInTheDocument();
    expect(within(panel).getByText('目标仓位变化')).toBeInTheDocument();
  });

  it('merges websocket live events into the selected track view', async () => {
    const { client, emit } = createFakeServerClient();

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    act(() => {
      emit(liveTrackEvent());
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(panel).getByText('策略价 102.75')).toBeInTheDocument();
    expect(within(panel).getByText('标记价 103.25')).toBeInTheDocument();
    expect(within(panel).getByText('revision 8')).toBeInTheDocument();
    expect(within(panel).getByText('submit_planned')).toBeInTheDocument();
    expect(within(panel).getByText('策略 9.0000')).toBeInTheDocument();
    expect(within(panel).getByText('风控 5.0000')).toBeInTheDocument();
    expect(within(panel).getByText('前沿 2.0000')).toBeInTheDocument();
    expect(within(panel).getByText('执行 1.7500')).toBeInTheDocument();
  });

  it('drops a stale live overlay when full list and detail events arrive', async () => {
    const { client, emit } = createFakeServerClient();

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    act(() => {
      emit(liveTrackEvent());
    });
    expect(screen.getByText('revision 8')).toBeInTheDocument();

    act(() => {
      emit({
        type: 'track_list_item_changed',
        track_id: 'btc-core',
        item: pausedTrackListItem(),
      });
    });

    expect(screen.queryByText('revision 8')).not.toBeInTheDocument();
    expect(screen.getByText('revision 7')).toBeInTheDocument();
    expect(screen.getByText('paused')).toBeInTheDocument();

    act(() => {
      emit(liveTrackEvent());
    });
    expect(screen.getByText('revision 8')).toBeInTheDocument();

    act(() => {
      emit({
        type: 'track_detail_changed',
        track_id: 'btc-core',
        detail: pausedTrackDetail(),
      });
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(panel).queryByText('revision 8')).not.toBeInTheDocument();
    expect(within(panel).getByText('revision --')).toBeInTheDocument();
    expect(within(panel).getAllByText('paused')).toHaveLength(2);
    expect(within(panel).getByText('策略 --')).toBeInTheDocument();
    expect(within(panel).getByText('执行 --')).toBeInTheDocument();
  });

  it('clears the previous server live cache when reconnecting with the same track id', async () => {
    const { client, emit } = createFakeServerClient();
    vi.mocked(client.listTracks).mockImplementation(async (baseUrl) => ({
      items: [baseUrl === 'http://127.0.0.1:9000' ? pausedTrackListItem() : trackListItem()],
    }));
    vi.mocked(client.getTrackDetail).mockImplementation(async (baseUrl) => (
      baseUrl === 'http://127.0.0.1:9000' ? pausedTrackDetail() : trackDetail()
    ));

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });
    act(() => {
      emit(liveTrackEvent());
    });
    expect(screen.getByText('revision 8')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Server URL'), {
      target: { value: 'http://127.0.0.1:9000/' },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(client.listTracks).toHaveBeenLastCalledWith('http://127.0.0.1:9000');
    expect(within(panel).queryByText('revision 8')).not.toBeInTheDocument();
    expect(within(panel).getByText('revision --')).toBeInTheDocument();
    expect(within(panel).getAllByText('paused')).toHaveLength(2);
    expect(within(panel).getByText('策略价 --')).toBeInTheDocument();
    expect(within(panel).getByText('标记价 --')).toBeInTheDocument();
  });

  it('shows risk acquisition backlog separately from the release frontier', async () => {
    const detail = trackDetail();
    detail.decision = {
      revision: 9,
      policy_target: 4.8235,
      risk_approved_target: 4.817,
      acquisition_frontier: 4.8,
      execution_target: 4.8,
      reason: 'waiting_risk_release',
    };
    detail.execution.risk_acquisition = {
      direction: 'long',
      backlog_units: 0.0235,
      release_anchor_price: 1518.3,
      release_anchor_target: 4.817,
      next_advantage_target: 5.517,
      next_advantage_price: 1448.3,
      next_release_units: 0.0235,
      next_release_target: 4.8235,
    };
    const { client } = createFakeServerClient(detail);

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(panel).getByText('策略 4.8235')).toBeInTheDocument();
    expect(within(panel).getByText('风控 4.8170')).toBeInTheDocument();
    expect(within(panel).getByText('前沿 4.8000')).toBeInTheDocument();
    expect(within(panel).getByText('执行 4.8000')).toBeInTheDocument();
    expect(within(panel).getByText('backlog +0.0235')).toBeInTheDocument();
    expect(within(panel).getByText('next +0.0235 → 4.8235')).toBeInTheDocument();
  });

  it('shows the fresh acquisition session without backlog and replaces it from live events', async () => {
    const detail = trackDetail();
    detail.execution.acquisition_session = {
      acquisition_reset_reason: 'fresh_session_start',
      acquisition_session_started_at: '2026-07-01T08:00:00+00:00',
      initial_frontier: null,
      initial_release_units: null,
    };
    const { client, emit } = createFakeServerClient(detail);

    render(<ServerLivePanel enabled client={client} />);

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '连接 Server' }));
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(panel).getByText('Acquisition session')).toBeInTheDocument();
    expect(within(panel).getByText('fresh_session_start')).toBeInTheDocument();
    expect(within(panel).getByText(
      'start 2026-07-01T08:00:00+00:00 · frontier -- · release --',
    )).toBeInTheDocument();

    const live = liveTrackEvent();
    live.live.acquisition_session = {
      acquisition_reset_reason: 'fresh_session_start',
      acquisition_session_started_at: '2026-07-01T08:00:00+00:00',
      initial_frontier: 2,
      initial_release_units: 0,
    };
    act(() => {
      emit(live);
    });

    expect(within(panel).getByText(
      'start 2026-07-01T08:00:00+00:00 · frontier 2.0000 · release +0.0000',
    )).toBeInTheDocument();
    expect(detail.execution.risk_acquisition).toBeUndefined();
  });
});

function createFakeServerClient(detail = trackDetail()) {
  let onEvent: ((event: StreamEvent) => void) | null = null;
  const client: PoiseServerClient = {
    listTracks: vi.fn(async () => ({
      items: [trackListItem()],
    })),
    getTrackDetail: vi.fn(async () => detail),
    getTrackDiagnostics: vi.fn(async () => trackDiagnostics()),
    connectEvents: vi.fn((_baseUrl, nextOnEvent) => {
      onEvent = nextOnEvent;
      return vi.fn();
    }),
  };

  return {
    client,
    emit(event: StreamEvent) {
      onEvent?.(event);
    },
  };
}

function trackListItem(): TrackListItemView {
  return {
    id: 'btc-core',
    instrument: {
      venue: 'binance',
      symbol: 'BTCUSDT',
    },
    lifecycle: {
      status: 'active',
      updated_at: '2026-05-08T00:00:00Z',
    },
    strategy_price: 100.5,
    strategy_price_status: 'live',
    exposure: {
      current: 1.5,
    },
    decision: {
      revision: 7,
      policy_target: 8,
      risk_approved_target: 4,
      acquisition_frontier: 1.5,
      execution_target: 1.25,
      reason: 'waiting_risk_release',
    },
    execution: {
      state: 'open',
      execution_status: 'normal',
      attention_reasons: [],
      price_execution_block_reason: null,
      active_binding_count: 1,
    },
    pnl: {
      pnl_asset: 'USDT',
      total_pnl: 12.3,
    },
  };
}

function trackDetail(): TrackDetailView {
  return {
    identity: {
      id: 'btc-core',
      instrument: {
        venue: 'binance',
        symbol: 'BTCUSDT',
      },
    },
    status: {
      lifecycle: {
        status: 'active',
        updated_at: '2026-05-08T00:00:00Z',
      },
      strategy_price: 100.5,
      strategy_price_status: 'live',
    },
    strategy: {
      lower_price: 90,
      upper_price: 110,
      long_exposure_units: 8,
      short_exposure_units: 8,
      notional_per_unit: 375,
      min_rebalance_units: 0.5,
      shape_family: 'linear',
      out_of_band_policy: 'freeze',
    },
    max_notional: 3000,
    loss_limits: {
      daily_loss_limit: 120,
      total_loss_limit: 500,
    },
    market: {
      mark_price: 101.25,
      best_bid: 101.2,
      best_ask: 101.3,
    },
    position: {
      current_exposure: 1.5,
    },
    decision: {
      revision: 7,
      policy_target: 8,
      risk_approved_target: 4,
      acquisition_frontier: 1.5,
      execution_target: 1.25,
      reason: 'waiting_risk_release',
    },
    pnl: {
      pnl_asset: 'USDT',
      gross_realized_pnl: 0,
      net_realized_pnl: 0,
      unrealized_pnl: 1,
      total_pnl: 12.3,
      trading_fee_cumulative: 0.2,
      funding_fee_cumulative: 0.1,
    },
    execution: {
      state: 'open',
      execution_status: 'normal',
      attention_reasons: [],
      price_execution_block_reason: null,
      active_binding_count: 1,
      bindings: [
        {
          id: 'binding-1',
          policy: 'curve_maker',
          label: 'lower rung',
          status: 'working',
          intent: 'increase_inventory',
          order: {
            side: 'buy',
            price: 100.2,
            quantity: 0.25,
          },
        },
      ],
    },
    activity: [
      {
        ts: '2026-05-08T00:00:00Z',
        level: 'info',
        message: '执行绑定已刷新',
      },
    ],
    available_commands: [],
  };
}

function trackDiagnostics(): TrackDiagnosticsView {
  return {
    items: [
      {
        ts: '2026-05-08T00:00:01Z',
        level: 'info',
        message: '目标仓位变化',
      },
    ],
  };
}

function liveTrackEvent(): Extract<StreamEvent, { type: 'track_live_view_changed' }> {
  return {
    type: 'track_live_view_changed',
    track_id: 'btc-core',
    live: {
      strategy_price: 102.75,
      strategy_price_status: 'live',
      mark_price: 103.25,
      best_bid: 103.1,
      best_ask: 103.3,
      decision: {
        revision: 8,
        policy_target: 9,
        risk_approved_target: 5,
        acquisition_frontier: 2,
        execution_target: 1.75,
        reason: 'submit_planned',
      },
      price_execution_block_reason: null,
    },
  };
}

function pausedTrackListItem(): TrackListItemView {
  const item = trackListItem();
  return {
    ...item,
    lifecycle: {
      ...item.lifecycle,
      status: 'paused',
      updated_at: '2026-05-08T00:00:02Z',
    },
    strategy_price: null,
    strategy_price_status: 'stale',
    decision: unavailableDecision(),
  };
}

function pausedTrackDetail(): TrackDetailView {
  const detail = trackDetail();
  return {
    ...detail,
    status: {
      ...detail.status,
      lifecycle: {
        ...detail.status.lifecycle,
        status: 'paused',
        updated_at: '2026-05-08T00:00:02Z',
      },
      strategy_price: null,
      strategy_price_status: 'stale',
    },
    market: {
      mark_price: null,
      best_bid: null,
      best_ask: null,
    },
    decision: unavailableDecision(),
    execution: {
      ...detail.execution,
      state: 'paused',
      active_binding_count: 0,
      bindings: [],
    },
  };
}

function unavailableDecision(): InventoryDecisionView {
  return {
    revision: null,
    policy_target: null,
    risk_approved_target: null,
    acquisition_frontier: null,
    execution_target: null,
    reason: 'paused',
  };
}
