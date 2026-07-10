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
import { bandProtectionKindFromPolicy, createTrackDraft } from '@/domain/trackDraft';

function riskAcquisitionFixture() {
  return {
    initialRatio: '0.5',
    advantageSteps: '2',
    minReleaseSteps: '1',
    maxReleaseSteps: '4',
    catchupRatio: '0.25',
    staleReleaseMinutes: '60',
  };
}

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

  it('hydrates the exchange venue without injecting partial exchange rules', async () => {
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
            risk_acquisition: {
              initial_ratio: 0.5,
              advantage_steps: 2,
              min_release_steps: 1,
              max_release_steps: 4,
              catchup_ratio: 0.25,
              stale_release_minutes: 60,
            },
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
    expect(loaded.projectedTracks[0].attachments).not.toHaveProperty('exchangeRules');
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
              risk_acquisition: {
              initial_ratio: 0.5,
                advantage_steps: 2,
                min_release_steps: 1,
                max_release_steps: 4,
                catchup_ratio: 0.25,
              stale_release_minutes: 60,
              },
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

  it('keeps risk acquisition parameters when loading and exporting the same draft', async () => {
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
              risk_acquisition: {
                initial_ratio: 0.5,
                advantage_steps: 2,
                min_release_steps: 1,
                max_release_steps: 4,
                catchup_ratio: 0.25,
                stale_release_minutes: 60,
              },
            },
            load_issues: [],
          },
        ],
      })
      .mockResolvedValueOnce('[[tracks]]');

    const bridge = createWorkbenchBridge();
    const loaded = await bridge.loadConfigFile('/tmp/strategies/grid.toml');

    expect(loaded.projectedTracks[0].riskAcquisition).toEqual({
      initialRatio: '0.5',
      advantageSteps: '2',
      minReleaseSteps: '1',
      maxReleaseSteps: '4',
      catchupRatio: '0.25',
      staleReleaseMinutes: '60',
    });

    await bridge.exportCurrentTrack(loaded.projectedTracks[0]);

    expect(invokeMock).toHaveBeenLastCalledWith('export_current_track', {
      draft: expect.objectContaining({
        fields: expect.objectContaining({
          risk_acquisition: {
            initial_ratio: 0.5,
            advantage_steps: 2,
            min_release_steps: 1,
            max_release_steps: 4,
            catchup_ratio: 0.25,
            stale_release_minutes: 60,
          },
        }),
      }),
    });
  });

  it('loads risk acquisition defaults from the backend', async () => {
    invokeMock.mockResolvedValueOnce({
      initial_ratio: 0.4,
      advantage_steps: 1.5,
      min_release_steps: 0.75,
      max_release_steps: 3,
      catchup_ratio: 0.2,
      stale_release_minutes: 20,
    });

    const bridge = createWorkbenchBridge();
    const defaults = await bridge.loadRiskAcquisitionDefaults();

    expect(invokeMock).toHaveBeenCalledWith('risk_acquisition_defaults');
    expect(defaults).toEqual({
      initialRatio: '0.4',
      advantageSteps: '1.5',
      minReleaseSteps: '0.75',
      maxReleaseSteps: '3',
      catchupRatio: '0.2',
      staleReleaseMinutes: '20',
    });
  });

  it('sends one snake-case request and consumes the camel-case Rust report unchanged', async () => {
    invokeMock.mockResolvedValueOnce({
      rules: {
        fixtureId: 'inverse_contract_v1',
        priceTick: 0.5,
        pricePrecision: { kind: 'fixed_tick' },
        quantityKind: 'inverse_contract',
        contractNotional: 100,
        settlementAsset: 'BTC',
        quantityStep: 1,
        minQty: 1,
        minNotional: 100,
        makerFeeRate: 0.0002,
        takerFeeRate: 0.0005,
        nativeQtyPerExposureUnit: 3,
      },
      assumptions: {
        rulesNotice: 'RULES_SENTINEL',
        fillModelNotice: 'FILL_SENTINEL',
        initialNativeQuantity: 0,
        initialExposure: 0,
      },
      curvePoints: [
        { price: 90, targetExposure: 7.125 },
      ],
      scenarios: [
        {
          id: 'price_advantage_sweep_long',
          title: 'SENTINEL_SCENARIO',
          direction: 'long',
          fillModel: 'immediate_full_at_order_price',
          horizon: {
            firstLiveAt: '2026-01-01T00:00:00Z',
            endedAt: '2026-01-01T00:03:00Z',
            wallClockDurationSeconds: 180,
            liveDurationSeconds: 180,
          },
          assumptions: {
            initialNativeQuantity: 0,
            initialExposure: 0,
            deterministicFillNotice: 'SCENARIO_ASSUMPTION',
          },
          curveImmediateBenchmark: {
            execution: {
              longRiskIncrease: { nativeQty: 11, weightedFillPrice: 91.25 },
              shortRiskIncrease: { nativeQty: 0, weightedFillPrice: null },
              turnoverNotional: 1100,
              estimatedTakerFee: 0.55,
              feeAsset: 'BTC',
              maxAbsNotional: 2121,
              maxMarginRequirement: 707,
            },
            tracking: {
              timeWeightedTrackingErrorUnits: 0.125,
              maxTrackingErrorUnits: 0.25,
            },
          },
          delayedDecisionReplay: {
            execution: {
              longRiskIncrease: { nativeQty: 5, weightedFillPrice: 88.75 },
              shortRiskIncrease: { nativeQty: 0, weightedFillPrice: null },
              turnoverNotional: 500,
              estimatedTakerFee: 0.25,
              feeAsset: 'BTC',
              maxAbsNotional: 989,
              maxMarginRequirement: 329.666,
            },
            tracking: {
              timeWeightedTrackingErrorUnits: 4.25,
              maxTrackingErrorUnits: 5.5,
            },
            backlog: {
              maxBacklogUnits: 6.5,
              timeWeightedMeanBacklogUnits: 3.25,
              maxAuthorizedButUnfilledGapUnits: 1.5,
              timeWeightedMeanAuthorizedButUnfilledGapUnits: 0.75,
              releaseCount: 2,
              maxSingleCatchUpUnits: 2.75,
            },
          },
          comparison: {
            curveImmediateTimeWeightedTrackingErrorUnits: 0.125,
            delayedTimeWeightedTrackingErrorUnits: 4.25,
            additionalDelayedTrackingErrorUnits: 4.125,
            curveImmediateMaxTrackingErrorUnits: 0.25,
            delayedMaxTrackingErrorUnits: 5.5,
            longRiskIncreasePrice: {
              curveImmediateTotalNativeQty: 11,
              delayedTotalNativeQty: 5,
              matchedNativeQty: 5,
              curveImmediateMatchedWeightedPrice: 91.25,
              delayedMatchedWeightedPrice: 88.75,
              favorablePriceImprovement: 2.5,
            },
            shortRiskIncreasePrice: {
              curveImmediateTotalNativeQty: 0,
              delayedTotalNativeQty: 0,
              matchedNativeQty: 0,
              curveImmediateMatchedWeightedPrice: null,
              delayedMatchedWeightedPrice: null,
              favorablePriceImprovement: null,
            },
          },
          replayRows: [
            {
              observedAt: '2026-01-01T00:01:00Z',
              action: 'execution_quote',
              strategyPrice: 95,
              simulatedPositionNativeQuantity: 5,
              simulatedPositionExposure: 1.75,
              observedPositionNativeQuantity: 4,
              observedPositionExposure: 1.5,
              signedFilledNativeQuantity: 5,
              policyTarget: 7.125,
              riskApprovedTarget: 3.5,
              acquisitionFrontier: 3.25,
              executionTarget: 3,
              reason: 'waiting_risk_release',
              revision: 77,
              appliedReleases: [
                {
                  trigger: 'price_advantage',
                  direction: 'long',
                  frontierBefore: 1.25,
                  frontierAfter: 3.25,
                  releaseUnits: 2,
                },
              ],
            },
          ],
        },
      ],
    });

    const draft = createTrackDraft({
      draftId: 'draft-btc',
      riskAcquisition: riskAcquisitionFixture(),
      raw: {
        trackId: 'btc',
        symbol: 'BTCUSD',
        lowerPrice: '90',
        upperPrice: '110',
        longExposureUnits: '8',
        shortExposureUnits: '8',
        notionalPerUnit: '300',
        maxNotional: '3000',
        minRebalanceUnits: '1',
        leverage: '3.7',
        dailyLossLimit: '120',
        totalLossLimit: '500',
        shapeFamily: 'linear',
        bandProtectionPolicy: 'freeze',
      },
    });

    const bridge = createWorkbenchBridge();
    const report = await bridge.evaluateTrack({
      draft,
      exchangeVenue: 'binance',
      rulesFixture: 'inverse_contract_v1',
    });

    expect(invokeMock).toHaveBeenCalledWith('evaluate_track', {
      request: {
        draft: expect.objectContaining({
          draft_id: 'draft-btc',
          fields: expect.objectContaining({
            track_id: 'btc',
            leverage: 3.7,
            risk_acquisition: expect.objectContaining({ initial_ratio: 0.5 }),
          }),
        }),
        exchange_venue: 'binance',
        rules_fixture: 'inverse_contract_v1',
      },
    });
    expect(report.rules).toMatchObject({
      fixtureId: 'inverse_contract_v1',
      quantityKind: 'inverse_contract',
      nativeQtyPerExposureUnit: 3,
    });
    expect(report.curvePoints).toEqual([{ price: 90, targetExposure: 7.125 }]);
    expect(report.scenarios[0]).toMatchObject({
      id: 'price_advantage_sweep_long',
      curveImmediateBenchmark: {
        execution: { maxAbsNotional: 2121 },
      },
      delayedDecisionReplay: {
        backlog: { maxBacklogUnits: 6.5 },
      },
      replayRows: [
        expect.objectContaining({
          policyTarget: 7.125,
          riskApprovedTarget: 3.5,
          acquisitionFrontier: 3.25,
          executionTarget: 3,
          reason: 'waiting_risk_release',
          revision: 77,
          appliedReleases: [expect.objectContaining({ releaseUnits: 2 })],
        }),
      ],
    });
  });

  it('surfaces an evaluation command error without a TypeScript fallback', async () => {
    invokeMock.mockRejectedValueOnce({
      kind: 'evaluation',
      message: 'EVALUATION_FIXTURE_SENTINEL',
    });
    const bridge = createWorkbenchBridge();
    const draft = createTrackDraft({
      draftId: 'draft-error',
      riskAcquisition: riskAcquisitionFixture(),
      raw: {
        trackId: 'error',
        symbol: 'BTCUSDT',
        lowerPrice: '90',
        upperPrice: '110',
        longExposureUnits: '8',
        shortExposureUnits: '8',
        notionalPerUnit: '100',
        maxNotional: '3000',
        minRebalanceUnits: '1',
        leverage: '3',
        dailyLossLimit: '120',
        totalLossLimit: '500',
        shapeFamily: 'linear',
        bandProtectionPolicy: 'freeze',
      },
    });

    await expect(bridge.evaluateTrack({
      draft,
      exchangeVenue: 'binance',
      rulesFixture: 'base_asset_v1',
    })).rejects.toThrow('EVALUATION_FIXTURE_SENTINEL');
    expect(invokeMock).toHaveBeenCalledTimes(1);
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

  it('does not provide a TypeScript evaluator fallback in browser preview', async () => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    const bridge = createWorkbenchBridge();
    const draft = createTrackDraft({
      draftId: 'draft-browser',
      riskAcquisition: riskAcquisitionFixture(),
      raw: {
        trackId: 'browser',
        symbol: 'BTCUSDT',
        lowerPrice: '90',
        upperPrice: '110',
        longExposureUnits: '8',
        shortExposureUnits: '8',
        notionalPerUnit: '100',
        maxNotional: '3000',
        minRebalanceUnits: '1',
        leverage: '3',
        dailyLossLimit: '120',
        totalLossLimit: '500',
        shapeFamily: 'linear',
        bandProtectionPolicy: 'freeze',
      },
    });

    await expect(bridge.evaluateTrack({
      draft,
      exchangeVenue: 'binance',
      rulesFixture: 'base_asset_v1',
    })).rejects.toThrow('Rust evaluator 不可用');
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('does not recreate Rust risk defaults in browser preview', async () => {
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;

    await expect(createWorkbenchBridge().loadRiskAcquisitionDefaults()).rejects.toThrow(
      'Rust defaults 不可用',
    );
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it('returns the camel-case Rust evaluation read model without a second field mapper', async () => {
    const payload = {
      rules: {
        fixtureId: 'base_asset_v1' as const,
        priceTick: 0.1,
        pricePrecision: { kind: 'fixed_tick' as const },
        quantityKind: 'base_asset' as const,
        contractNotional: null,
        settlementAsset: 'USDT',
        quantityStep: 0.001,
        minQty: 0,
        minNotional: 0,
        makerFeeRate: 0.0002,
        takerFeeRate: 0.001,
        nativeQtyPerExposureUnit: 1,
      },
      assumptions: {
        rulesNotice: 'RULES',
        fillModelNotice: 'FILL',
        initialNativeQuantity: 0,
        initialExposure: 0,
      },
      curvePoints: [],
      scenarios: [],
    };
    invokeMock.mockResolvedValueOnce(payload);
    const draft = createTrackDraft({
      draftId: 'camel-report',
      riskAcquisition: riskAcquisitionFixture(),
      raw: {
        trackId: 'camel-report',
        symbol: 'BTCUSDT',
        lowerPrice: '90',
        upperPrice: '110',
        longExposureUnits: '8',
        shortExposureUnits: '8',
        notionalPerUnit: '100',
        maxNotional: '800',
        minRebalanceUnits: '1',
        leverage: '3',
        dailyLossLimit: '120',
        totalLossLimit: '500',
        shapeFamily: 'linear',
        bandProtectionPolicy: 'freeze',
      },
    });

    const report = await createWorkbenchBridge().evaluateTrack({
      draft,
      exchangeVenue: 'binance',
      rulesFixture: 'base_asset_v1',
    });

    expect(report).toBe(payload);
  });
});
