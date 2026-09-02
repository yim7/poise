import { act, renderHook, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type {
  WorkbenchBridge,
  WorkbenchEvaluationReport,
} from '@/app/workbenchBridge';
import { createTrackDraft } from '@/domain/trackDraft';
import { useTrackEvaluation } from '@/ui/app/useTrackEvaluation';

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function makeDraft(initialRatio = '0.5', quotePriceInput = '') {
  return createTrackDraft({
    draftId: 'draft-btc',
    raw: {
      trackId: 'btc',
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
    riskAcquisition: {
      initialRatio,
      advantageSteps: '2',
      minReleaseSteps: '1',
      maxReleaseSteps: '4',
      catchupRatio: '0.25',
      staleReleaseMinutes: '60',
    },
    ui: { quotePriceInput },
  });
}

function report(id: string): WorkbenchEvaluationReport {
  return {
    instrumentFixture: {
      fixtureId: 'base_asset_v1',
      baseAsset: 'BTC',
      quoteAsset: 'USDT',
      notionalAsset: 'USDT',
      priceTick: 0.1,
      pricePrecision: { kind: 'fixed_tick' },
      positionBasis: 'base_asset',
      contractValue: null,
      settlementAsset: 'USDT',
      positionQtyStep: 0.001,
      minPositionQty: 0.001,
      minNotional: 5,
      makerFeeRate: 0.0002,
      takerFeeRate: 0.0005,
      positionQtyPerExposureUnit: 1,
    },
    assumptions: {
      instrumentFixtureNotice: id,
      fillModelNotice: 'deterministic',
      initialPositionQty: 0,
      initialExposure: 0,
    },
    curvePoints: [],
    scenarios: [],
  };
}

function bridge(evaluateTrack: WorkbenchBridge['evaluateTrack']): WorkbenchBridge {
  return {
    isTauriEnvironment: () => true,
    openConfigFile: vi.fn(async () => null),
    loadConfigFile: vi.fn(),
    loadSavedDraft: vi.fn(async () => null),
    saveDraft: vi.fn(async () => {}),
    loadRiskAcquisitionDefaults: vi.fn(async () => ({
      initialRatio: '0.5',
      advantageSteps: '2',
      minReleaseSteps: '1',
      maxReleaseSteps: '4',
      catchupRatio: '0.25',
      staleReleaseMinutes: '60',
    })),
    exportCurrentTrack: vi.fn(),
    exportAllTracks: vi.fn(),
    copyText: vi.fn(async () => {}),
    fetchBinanceQuote: vi.fn(),
    evaluateTrack,
  };
}

describe('useTrackEvaluation', () => {
  it('requires an explicit instrument fixture before invoking Rust', async () => {
    const evaluateTrack = vi.fn(async () => report('MUST_NOT_RUN'));
    const resolvedBridge = bridge(evaluateTrack);
    const { result } = renderHook(() => useTrackEvaluation({
      bridge: resolvedBridge,
      draft: makeDraft(),
      exchangeVenue: 'binance',
      instrumentFixture: null,
    }));

    await waitFor(() => expect(result.current.status).toBe('blocked'));
    expect(result.current.message).toContain('请选择');
    expect(evaluateTrack).not.toHaveBeenCalled();
  });

  it('keeps only the newest response when requests resolve out of order', async () => {
    const first = deferred<WorkbenchEvaluationReport>();
    const second = deferred<WorkbenchEvaluationReport>();
    const evaluateTrack = vi.fn()
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(second.promise);
    const resolvedBridge = bridge(evaluateTrack);
    const { result, rerender } = renderHook(
      ({ draft }) => useTrackEvaluation({
        bridge: resolvedBridge,
        draft,
        exchangeVenue: 'binance',
        instrumentFixture: 'base_asset_v1',
      }),
      { initialProps: { draft: makeDraft('0.5') } },
    );

    await waitFor(() => expect(evaluateTrack).toHaveBeenCalledTimes(1));
    rerender({ draft: makeDraft('0.75') });
    await waitFor(() => expect(evaluateTrack).toHaveBeenCalledTimes(2));

    await act(async () => second.resolve(report('NEW_REPORT')));
    await waitFor(() => expect(result.current.report?.assumptions.instrumentFixtureNotice).toBe('NEW_REPORT'));
    await act(async () => first.resolve(report('STALE_REPORT')));

    expect(result.current.status).toBe('ready');
    expect(result.current.report?.assumptions.instrumentFixtureNotice).toBe('NEW_REPORT');
  });

  it('does not block synthetic evaluation on a missing or invalid manual quote', async () => {
    const evaluateTrack = vi.fn(async () => report('QUOTE_INDEPENDENT'));
    const resolvedBridge = bridge(evaluateTrack);
    const { result, rerender } = renderHook(
      ({ draft }) => useTrackEvaluation({
        bridge: resolvedBridge,
        draft,
        exchangeVenue: 'binance',
        instrumentFixture: 'base_asset_v1',
      }),
      { initialProps: { draft: makeDraft('0.5', '') } },
    );

    await waitFor(() => expect(evaluateTrack).toHaveBeenCalledTimes(1));
    rerender({ draft: makeDraft('0.75', 'not-a-price') });
    await waitFor(() => expect(evaluateTrack).toHaveBeenCalledTimes(2));

    const invalidStrategy = makeDraft('0.75', 'not-a-price');
    invalidStrategy.rawNumbers.dailyLossLimit = '0';
    rerender({ draft: invalidStrategy });
    await waitFor(() => expect(result.current.status).toBe('blocked'));
    expect(evaluateTrack).toHaveBeenCalledTimes(2);
  });

  it('blocks strategy errors and clears the previous report while a new request loads or fails', async () => {
    const next = deferred<WorkbenchEvaluationReport>();
    const evaluateTrack = vi.fn()
      .mockResolvedValueOnce(report('OLD_REPORT'))
      .mockReturnValueOnce(next.promise);
    const resolvedBridge = bridge(evaluateTrack);
    const { result, rerender } = renderHook(
      ({ draft }) => useTrackEvaluation({
        bridge: resolvedBridge,
        draft,
        exchangeVenue: 'binance',
        instrumentFixture: 'base_asset_v1',
      }),
      { initialProps: { draft: makeDraft('0.5') } },
    );

    await waitFor(() => expect(result.current.report?.assumptions.instrumentFixtureNotice).toBe('OLD_REPORT'));
    rerender({ draft: makeDraft('0.75') });
    await waitFor(() => expect(result.current.status).toBe('loading'));
    expect(result.current.report).toBeNull();

    await act(async () => next.reject(new Error('EVALUATION_SENTINEL_ERROR')));
    await waitFor(() => expect(result.current.status).toBe('error'));
    expect(result.current.report).toBeNull();
    expect(result.current.message).toBe('EVALUATION_SENTINEL_ERROR');

    const invalid = makeDraft('0.75');
    invalid.rawNumbers.lowerPrice = 'oops';
    rerender({ draft: invalid });
    await waitFor(() => expect(result.current.status).toBe('blocked'));
    expect(evaluateTrack).toHaveBeenCalledTimes(2);
  });
});
