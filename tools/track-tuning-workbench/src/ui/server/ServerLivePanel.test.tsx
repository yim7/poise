import '@testing-library/jest-dom/vitest';

import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type {
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
    expect(within(panel).getByText('目标 2.0000')).toBeInTheDocument();
    expect(within(panel).getByText('差额 0.5000')).toBeInTheDocument();
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
      emit({
        type: 'track_live_view_changed',
        track_id: 'btc-core',
        live: {
          strategy_price: 102.75,
          strategy_price_status: 'live',
          mark_price: 103.25,
          best_bid: 103.1,
          best_ask: 103.3,
          desired_exposure: 4,
          price_execution_block_reason: null,
        },
      });
    });

    const panel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(panel).getByText('策略价 102.75')).toBeInTheDocument();
    expect(within(panel).getByText('标记价 103.25')).toBeInTheDocument();
    expect(within(panel).getByText('目标 4.0000')).toBeInTheDocument();
  });

  it('shows risk acquisition backlog separately from the release frontier', async () => {
    const detail = trackDetail();
    detail.position.desired_exposure = 1.5;
    detail.execution.inventory_gap = 0;
    detail.execution.risk_acquisition = {
      direction: 'long',
      curve_target: 4.8235,
      risk_release_frontier: 4.8,
      backlog_units: 0.0235,
      anchor_price: 1518.3,
      anchor_curve_target: 4.817,
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
    expect(within(panel).getByText('释放 4.8000 · 曲线 4.8235')).toBeInTheDocument();
    expect(within(panel).getByText('backlog +0.0235')).toBeInTheDocument();
    expect(within(panel).getByText('next +0.0235 → 4.8235')).toBeInTheDocument();
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
      target: 2,
    },
    execution: {
      state: 'open',
      execution_status: 'normal',
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
      desired_exposure: 2,
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
      inventory_gap: 0.5,
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
