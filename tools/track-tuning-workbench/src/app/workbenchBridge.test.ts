import '@testing-library/jest-dom/vitest';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const openMock = vi.fn();
const invokeMock = vi.fn();

vi.mock('@tauri-apps/plugin-dialog', () => ({
  open: openMock,
}));

vi.mock('@tauri-apps/api/core', () => ({
  invoke: invokeMock,
}));

import { createWorkbenchBridge } from '@/app/workbenchBridge';

describe('createWorkbenchBridge', () => {
  beforeEach(() => {
    Object.defineProperty(window, '__TAURI_INTERNALS__', {
      value: {},
      configurable: true,
    });
    openMock.mockReset();
    invokeMock.mockReset();
  });

  afterEach(() => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  });

  it('uses the async dialog plugin instead of a blocking backend command when choosing config files', async () => {
    openMock.mockResolvedValue('/tmp/strategies/grid.toml');

    const bridge = createWorkbenchBridge();
    const selectedPath = await bridge.openConfigFile();

    expect(selectedPath).toBe('/tmp/strategies/grid.toml');
    expect(openMock).toHaveBeenCalledWith({
      directory: false,
      filters: [
        {
          name: 'TOML',
          extensions: ['toml'],
        },
      ],
      multiple: false,
      title: '选择 Track 配置文件',
    });
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('hydrates loaded tracks with default Binance futures fee rates', async () => {
    invokeMock.mockResolvedValueOnce({
      config_path: '/tmp/strategies/grid.toml',
      projected_tracks: [
        {
          draft_id: 'draft-btc',
          fields: {
            track_id: 'btc',
            symbol: 'BTCUSDT',
            lower_price: 65000,
            upper_price: 70000,
            long_exposure_units: 8,
            short_exposure_units: 8,
            notional_per_unit: 250,
            max_notional: 3000,
            min_rebalance_units: 0.5,
            leverage: 10,
            out_of_band_policy: {
              freeze: {
                recover: 'back_in_band',
              },
            },
            daily_loss_limit: 120,
            total_loss_limit: 500,
            shape_family: 'linear',
          },
          load_issues: [],
        },
      ],
    });

    const bridge = createWorkbenchBridge();
    const loaded = await bridge.loadConfigFile('/tmp/strategies/grid.toml');

    expect(loaded.projectedTracks).toHaveLength(1);
    expect(loaded.projectedTracks[0].enums.bandProtectionKind).toBe('freeze');
    expect(loaded.projectedTracks[0].attachments.exchangeRules).toEqual({
      makerFeeRate: 0.0002,
      takerFeeRate: 0.0005,
    });
  });
});
