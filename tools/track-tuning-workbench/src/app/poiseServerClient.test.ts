import { describe, expect, it, vi } from 'vitest';

import {
  createPoiseServerClient,
  normalizeServerBaseUrl,
  serverWebSocketUrl,
  type StreamEvent,
} from '@/app/poiseServerClient';

describe('poiseServerClient', () => {
  it('normalizes server base urls before requesting the track list', async () => {
    const fetchJson = vi.fn(async () =>
      new Response(
        JSON.stringify({
          items: [],
        }),
        {
          status: 200,
          headers: {
            'content-type': 'application/json',
          },
        },
      ),
    );
    const client = createPoiseServerClient({ fetch: fetchJson });

    await expect(client.listTracks(' http://127.0.0.1:8000/ ')).resolves.toEqual({
      items: [],
    });

    expect(fetchJson).toHaveBeenCalledWith('http://127.0.0.1:8000/tracks', {
      headers: {
        accept: 'application/json',
      },
    });
  });

  it('converts server http urls to websocket urls', () => {
    expect(serverWebSocketUrl('http://127.0.0.1:8000')).toBe('ws://127.0.0.1:8000/ws');
    expect(serverWebSocketUrl('https://poise.local/api/')).toBe('wss://poise.local/api/ws');
  });

  it('url-encodes track ids for detail and diagnostics requests', async () => {
    const fetchJson = vi.fn(async () =>
      new Response(
        JSON.stringify({
          identity: {
            id: 'btc/grid',
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
            strategy_price: 100,
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
            mark_price: 100,
            best_bid: 99.9,
            best_ask: 100.1,
          },
          position: {
            current_exposure: 1,
            desired_exposure: 2,
          },
          pnl: {
            pnl_asset: 'USDT',
            gross_realized_pnl: 0,
            net_realized_pnl: 0,
            unrealized_pnl: 0,
            total_pnl: 0,
            trading_fee_cumulative: 0,
            funding_fee_cumulative: 0,
          },
          execution: {
            state: 'open',
            execution_status: 'normal',
            attention_reasons: [],
            inventory_gap: 1,
            active_binding_count: 0,
            bindings: [],
          },
          activity: [],
          available_commands: [],
        }),
        {
          status: 200,
          headers: {
            'content-type': 'application/json',
          },
        },
      ),
    );
    const client = createPoiseServerClient({ fetch: fetchJson });

    await client.getTrackDetail('http://127.0.0.1:8000/', 'btc/grid');
    await client.getTrackDiagnostics('http://127.0.0.1:8000/', 'btc/grid');

    expect(fetchJson).toHaveBeenNthCalledWith(
      1,
      'http://127.0.0.1:8000/tracks/btc%2Fgrid',
      expect.any(Object),
    );
    expect(fetchJson).toHaveBeenNthCalledWith(
      2,
      'http://127.0.0.1:8000/debug/tracks/btc%2Fgrid/diagnostics',
      expect.any(Object),
    );
  });

  it('parses websocket event payloads and exposes a cleanup function', () => {
    const sockets: FakeWebSocket[] = [];
    const client = createPoiseServerClient({
      WebSocket: class extends FakeWebSocket {
        constructor(url: string) {
          super(url);
          sockets.push(this);
        }
      },
    });
    const onEvent = vi.fn();

    const disconnect = client.connectEvents(
      normalizeServerBaseUrl('http://127.0.0.1:8000'),
      onEvent,
    );
    sockets[0].emitMessage({
      type: 'track_live_view_changed',
      track_id: 'btc-core',
      live: {
        strategy_price: 100,
        strategy_price_status: 'live',
        mark_price: 100.2,
        best_bid: 100.1,
        best_ask: 100.3,
        desired_exposure: 3,
        price_execution_block_reason: null,
      },
    });
    disconnect();

    expect(sockets[0].url).toBe('ws://127.0.0.1:8000/ws');
    expect(onEvent).toHaveBeenCalledWith({
      type: 'track_live_view_changed',
      track_id: 'btc-core',
      live: {
        strategy_price: 100,
        strategy_price_status: 'live',
        mark_price: 100.2,
        best_bid: 100.1,
        best_ask: 100.3,
        desired_exposure: 3,
        price_execution_block_reason: null,
      },
    } satisfies StreamEvent);
    expect(sockets[0].closed).toBe(true);
  });
});

class FakeWebSocket extends EventTarget {
  readonly url: string;
  closed = false;

  constructor(url: string) {
    super();
    this.url = url;
  }

  close() {
    this.closed = true;
  }

  emitMessage(payload: StreamEvent) {
    this.dispatchEvent(new MessageEvent('message', {
      data: JSON.stringify(payload),
    }));
  }
}
