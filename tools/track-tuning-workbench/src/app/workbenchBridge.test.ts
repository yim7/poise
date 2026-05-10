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
import { bandProtectionKindFromPolicy } from '@/domain/trackDraft';

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
      exchange_venue: 'okx',
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
            out_of_band_policy: 'freeze',
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
    expect(
      bandProtectionKindFromPolicy(loaded.projectedTracks[0].enums.bandProtectionPolicy),
    ).toBe('freeze');
    expect('bandProtectionKind' in loaded.projectedTracks[0].enums).toBe(false);
    expect(loaded.projectedTracks[0].attachments.exchangeRules).toEqual({
      makerFeeRate: 0.0002,
      takerFeeRate: 0.0005,
    });
    expect(loaded.projectedTracks[0].attachments.exchangeVenue).toBe('okx');
  });

  it('keeps non-default flatten parameters when loading and exporting the same draft', async () => {
    invokeMock
      .mockResolvedValueOnce({
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
                flatten: {
                  trigger: {
                    flatten_confirm: { bps: 125 },
                  },
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
      })
      .mockResolvedValueOnce('[[tracks]]');

    const bridge = createWorkbenchBridge();
    const loaded = await bridge.loadConfigFile('/tmp/strategies/grid.toml');
    await bridge.exportCurrentTrack(loaded.projectedTracks[0]);

    expect(invokeMock).toHaveBeenLastCalledWith('export_current_track', {
      draft: expect.objectContaining({
        fields: expect.objectContaining({
          out_of_band_policy: {
            flatten: {
              trigger: {
                flatten_confirm: { bps: 125 },
              },
              recover: 'back_in_band',
            },
          },
        }),
      }),
    });
  });

  it('keeps risk increase delay parameters when loading and exporting the same draft', async () => {
    invokeMock
      .mockResolvedValueOnce({
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
              out_of_band_policy: 'freeze',
              daily_loss_limit: 120,
              total_loss_limit: 500,
              shape_family: 'linear',
              risk_increase_delay: {
                startup_initial_ratio: 0.3,
                advantage_min_rebalance_multiples: 2,
                base_step_min_rebalance_multiples: 1,
                max_step_min_rebalance_multiples: 4,
                catchup_ratio: 0.25,
              },
            },
            load_issues: [],
          },
        ],
      })
      .mockResolvedValueOnce('[[tracks]]');

    const bridge = createWorkbenchBridge();
    const loaded = await bridge.loadConfigFile('/tmp/strategies/grid.toml');

    expect(loaded.projectedTracks[0].riskIncreaseDelay).toEqual({
      startupInitialRatio: '0.3',
      advantageMinRebalanceMultiples: '2',
      baseStepMinRebalanceMultiples: '1',
      maxStepMinRebalanceMultiples: '4',
      catchupRatio: '0.25',
    });

    await bridge.exportCurrentTrack(loaded.projectedTracks[0]);

    expect(invokeMock).toHaveBeenLastCalledWith('export_current_track', {
      draft: expect.objectContaining({
        fields: expect.objectContaining({
          risk_increase_delay: {
            startup_initial_ratio: 0.3,
            advantage_min_rebalance_multiples: 2,
            base_step_min_rebalance_multiples: 1,
            max_step_min_rebalance_multiples: 4,
            catchup_ratio: 0.25,
          },
        }),
      }),
    });
  });

  it('loads risk increase delay defaults from the backend', async () => {
    invokeMock.mockResolvedValueOnce({
      startup_initial_ratio: 0.4,
      advantage_min_rebalance_multiples: 1.5,
      base_step_min_rebalance_multiples: 0.75,
      max_step_min_rebalance_multiples: 3,
      catchup_ratio: 0.2,
    });

    const bridge = createWorkbenchBridge();
    const defaults = await bridge.loadRiskIncreaseDelayDefaults();

    expect(invokeMock).toHaveBeenCalledWith('risk_increase_delay_defaults');
    expect(defaults).toEqual({
      startupInitialRatio: '0.4',
      advantageMinRebalanceMultiples: '1.5',
      baseStepMinRebalanceMultiples: '0.75',
      maxStepMinRebalanceMultiples: '3',
      catchupRatio: '0.2',
    });
  });

  it('keeps browser preview out of real config loading', async () => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    const createElementSpy = vi.spyOn(document, 'createElement');

    const bridge = createWorkbenchBridge();
    const selectedPath = await bridge.openConfigFile();

    expect(selectedPath).toBeNull();
    expect(createElementSpy).not.toHaveBeenCalled();
    await expect(bridge.loadConfigFile('grid.toml')).rejects.toThrow(
      '浏览器预览不读取真实配置文件',
    );

    createElementSpy.mockRestore();
  });

  it('keeps browser preview out of real exchange quotes', async () => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    const fetchMock = vi.spyOn(globalThis, 'fetch');

    const bridge = createWorkbenchBridge();
    const quote = await bridge.fetchBinanceQuote({
      exchangeVenue: 'binance',
      symbol: 'ETH',
    });

    expect(quote.price).toBeNull();
    expect(quote.errorKind).toBe('temporarily_unavailable');
    expect(quote.errorMessage).toContain('浏览器预览不连接交易所报价');
    expect(fetchMock).not.toHaveBeenCalled();

    fetchMock.mockRestore();
  });
});
