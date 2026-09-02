import '@testing-library/jest-dom/vitest';

import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { AppShell } from '@/app/AppShell';
import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';
import type { WorkbenchBridge } from '@/app/workbenchBridge';
import type { WorkbenchEvaluationReport } from '@/app/workbenchBridge';

interface DraftOverrides {
  additional?: TrackDraft['additional'];
  enums?: TrackDraft['enums'];
  rawNumbers?: Partial<TrackDraft['rawNumbers']>;
  riskAcquisition?: TrackDraft['riskAcquisition'];
  ui?: Partial<TrackDraft['ui']>;
  attachments?: TrackDraft['attachments'];
}

function makeDraft(draftId: string, overrides: DraftOverrides = {}): TrackDraft {
  return createTrackDraft({
    draftId,
    raw: {
      trackId: overrides.additional?.trackId ?? draftId,
      symbol: overrides.additional?.symbol ?? `${draftId.toUpperCase()}USDT`,
      lowerPrice: overrides.rawNumbers?.lowerPrice ?? '90',
      upperPrice: overrides.rawNumbers?.upperPrice ?? '110',
      longExposureUnits: overrides.rawNumbers?.longExposureUnits ?? '8',
      shortExposureUnits: overrides.rawNumbers?.shortExposureUnits ?? '8',
      notionalPerUnit: overrides.rawNumbers?.notionalPerUnit ?? '375',
      maxNotional: overrides.rawNumbers?.maxNotional ?? '3000',
      minRebalanceUnits: overrides.rawNumbers?.minRebalanceUnits ?? '0.5',
      leverage: overrides.rawNumbers?.leverage ?? '10',
      dailyLossLimit: overrides.rawNumbers?.dailyLossLimit ?? '120',
      totalLossLimit: overrides.rawNumbers?.totalLossLimit ?? '500',
      shapeFamily: overrides.enums?.shapeFamily ?? 'linear',
      bandProtectionPolicy: overrides.enums?.bandProtectionPolicy ?? 'freeze',
    },
    additional: overrides.additional,
    enums: overrides.enums,
    riskAcquisition: overrides.riskAcquisition ?? {
      initialRatio: '0.5',
      advantageSteps: '2',
      minReleaseSteps: '1',
      maxReleaseSteps: '4',
      catchupRatio: '0.25',
      staleReleaseMinutes: '60',
    },
    ui: {
      quotePriceInput: overrides.ui?.quotePriceInput ?? '100',
    },
    attachments: overrides.attachments,
  });
}


async function renderShell() {
  const snapshot = {
    selectedDraftId: 'draft-silver',
    drafts: [
      makeDraft('draft-silver', {
        additional: {
          trackId: 'silver',
          symbol: 'SILVERUSDT',
        },
        attachments: {
          currentExposure: 2.25,
        },
      }),
      makeDraft('draft-gold', {
        additional: {
          trackId: 'gold',
          symbol: 'GOLDUSDT',
        },
        rawNumbers: {
          lowerPrice: '190',
          upperPrice: '230',
        },
        ui: {
          quotePriceInput: '206',
        },
      }),
    ],
    temporaryPriceOverrides: {},
  };
  const store = createWorkbenchStore({ initialSnapshot: snapshot });

  await act(async () => {
    await store.load('/tmp/strategies/metals.toml', snapshot);
  });

  render(
    <WorkbenchStoreProvider store={store}>
      <AppShell />
    </WorkbenchStoreProvider>,
  );

  return { store };
}

function createMockBridge(
  overrides: Partial<WorkbenchBridge> = {},
): WorkbenchBridge {
  return {
    isTauriEnvironment: () => false,
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
    exportCurrentTrack: vi.fn(async () => '[[tracks]]\ntrack_id = "silver"'),
    exportAllTracks: vi.fn(async () => '[[tracks]]\ntrack_id = "silver"\n\n[[tracks]]\ntrack_id = "gold"'),
    copyText: vi.fn(async () => {}),
    fetchBinanceQuote: vi.fn(async () => ({
      price: '101.25',
      retrievedAt: 1_713_400_000_000,
      errorKind: null,
      errorMessage: null,
    })),
    evaluateTrack: vi.fn(async () => sentinelEvaluationReport()),
    ...overrides,
  };
}

function sentinelEvaluationReport(): WorkbenchEvaluationReport {
  const immediateExecution = {
    longRiskIncrease: { positionQty: 11, weightedFillPrice: 91.25 },
    shortRiskIncrease: { positionQty: 0, weightedFillPrice: null },
    turnoverNotional: 1100,
    estimatedTakerFee: 0.55,
    feeAsset: 'USDT',
    maxAbsNotional: 2121,
    maxMarginRequirement: 707,
  };
  const delayedExecution = {
    longRiskIncrease: { positionQty: 5, weightedFillPrice: 88.75 },
    shortRiskIncrease: { positionQty: 0, weightedFillPrice: null },
    turnoverNotional: 500,
    estimatedTakerFee: 0.25,
    feeAsset: 'USDT',
    maxAbsNotional: 989,
    maxMarginRequirement: 329.666,
  };
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
      positionQtyPerExposureUnit: 0.123456,
    },
    assumptions: {
      instrumentFixtureNotice: 'RULES_SENTINEL：确定性评估假设，不是实时规则',
      fillModelNotice: 'FILL_SENTINEL：不是成交概率、滑点或 PnL 预测',
      initialPositionQty: 0,
      initialExposure: 0,
    },
    curvePoints: [
      { price: 90, targetExposure: 7.125 },
      { price: 100, targetExposure: 0.375 },
      { price: 110, targetExposure: -6.875 },
    ],
    scenarios: [
      {
        id: 'price_advantage_sweep_long',
        title: 'SENTINEL 价格优势 long',
        direction: 'long',
        fillModel: 'immediate_full_at_order_price',
        horizon: {
          firstLiveAt: '2026-01-01T00:00:00Z',
          endedAt: '2026-01-01T00:03:00Z',
          wallClockDurationSeconds: 180,
          liveDurationSeconds: 180,
        },
        assumptions: {
          initialPositionQty: 0,
          initialExposure: 0,
          deterministicFillNotice: 'SCENARIO_FILL_SENTINEL',
        },
        curveImmediateBenchmark: {
          execution: immediateExecution,
          tracking: {
            timeWeightedTrackingErrorUnits: 0.125,
            maxTrackingErrorUnits: 0.25,
          },
        },
        delayedDecisionReplay: {
          execution: delayedExecution,
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
            curveImmediateTotalPositionQty: 11,
            delayedTotalPositionQty: 5,
            matchedPositionQty: 5,
            curveImmediateMatchedWeightedPrice: 91.25,
            delayedMatchedWeightedPrice: 88.75,
            favorablePriceImprovement: 2.5,
          },
          shortRiskIncreasePrice: {
            curveImmediateTotalPositionQty: 0,
            delayedTotalPositionQty: 0,
            matchedPositionQty: 0,
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
            simulatedPositionQty: 5,
            simulatedPositionExposure: 1.75,
            observedPositionQty: 4,
            observedPositionExposure: 1.5,
            signedFilledPositionQty: 5,
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
  };
}

describe('AppShell', () => {
  it('renders the primary actions and the Rust evaluator boundary for the selected track', async () => {
    await renderShell();

    expect(screen.getByRole('region', { name: '文件操作区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Track 列表区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Server live inspector' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '关键指标区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '主图区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '参数编辑区' })).toBeInTheDocument();

    expect(screen.getByRole('button', { name: '选择配置文件' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '撤销' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '重做' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '复制当前 Track' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '复制全部 Tracks' })).toBeInTheDocument();

    const metricRegion = screen.getByRole('region', { name: '关键指标区' });
    expect(within(metricRegion).getByText('曲线立即执行与延迟决策对照')).toBeInTheDocument();
    expect(within(metricRegion).getByLabelText('评估标的 fixture')).toBeInTheDocument();
    expect(within(metricRegion).getByText(/Rust evaluator 不可用/)).toBeInTheDocument();
  });

  it('keeps server live disabled outside the Tauri bridge', async () => {
    await renderShell();

    const serverPanel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(serverPanel).getByText('浏览器预览模式')).toBeInTheDocument();
    expect(within(serverPanel).getByText('server live 只在 Tauri 桌面版启用。')).toBeInTheDocument();
  });

  it('enables server live when AppShell receives a Tauri bridge', async () => {
    const bridge = createMockBridge({
      isTauriEnvironment: () => true,
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: '',
        drafts: [],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      render(
        <WorkbenchStoreProvider store={store}>
          <AppShell bridge={bridge} />
        </WorkbenchStoreProvider>,
      );
    });

    const serverPanel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(serverPanel).queryByText('浏览器预览模式')).toBeNull();
    expect(within(serverPanel).getByRole('button', { name: '连接 Server' })).toBeEnabled();
  });

  it('renders Rust sentinel assumptions, independent lanes, and delayed decision rows', async () => {
    const bridge = createMockBridge({ isTauriEnvironment: () => true });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-sentinel',
        drafts: [makeDraft('draft-sentinel', { ui: { quotePriceInput: '' } })],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      render(
        <WorkbenchStoreProvider store={store}>
          <AppShell bridge={bridge} />
        </WorkbenchStoreProvider>,
      );
    });

    expect(screen.getByLabelText('评估标的 fixture')).toHaveValue('');
    expect(bridge.evaluateTrack).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('评估标的 fixture'), {
      target: { value: 'base_asset_v1' },
    });
    expect(await screen.findByText(/RULES_SENTINEL/)).toBeInTheDocument();
    expect(screen.getAllByText(/FILL_SENTINEL/).length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText('tick 0.1000')).toBeInTheDocument();
    expect(screen.getByText(/position qty step 0\.0010/)).toBeInTheDocument();
    expect(screen.getByText('qty 0.0010')).toBeInTheDocument();
    expect(screen.getByText(/notional 5\.0000/)).toBeInTheDocument();
    expect(screen.getByText('base asset quantity')).toBeInTheDocument();
    expect(screen.getByText('base BTC · quote USDT · settlement USDT')).toBeInTheDocument();

    const immediate = screen.getByRole('region', { name: 'Curve-immediate benchmark' });
    const delayed = screen.getByRole('region', { name: 'Delayed decision replay' });
    expect(within(immediate).getByText('2,121.0000')).toBeInTheDocument();
    expect(within(immediate).getByText('position qty 11.0000')).toBeInTheDocument();
    expect(within(immediate).getByText('weighted price 91.2500')).toBeInTheDocument();
    expect(within(delayed).getByText('989.0000')).toBeInTheDocument();
    expect(within(delayed).getByText('position qty 5.0000')).toBeInTheDocument();
    expect(within(delayed).getByText('weighted price 88.7500')).toBeInTheDocument();
    expect(within(delayed).getByText('6.5000')).toBeInTheDocument();
    expect(within(delayed).getByText('policy 7.1250')).toBeInTheDocument();
    expect(within(delayed).getByText('risk approved 3.5000')).toBeInTheDocument();
    expect(within(delayed).getByText('frontier 3.2500')).toBeInTheDocument();
    expect(within(delayed).getByText('execution 3.0000')).toBeInTheDocument();
    expect(within(delayed).getByText('waiting_risk_release')).toBeInTheDocument();
    expect(within(delayed).getByText('revision 77')).toBeInTheDocument();
    expect(screen.getByText(/matched position qty 5\.0000/)).toBeInTheDocument();
    expect(screen.getByText(/immediate matched weighted price 91\.2500/)).toBeInTheDocument();
    expect(screen.getByText(/delayed matched weighted price 88\.7500/)).toBeInTheDocument();
    expect(screen.getByText(/favorable improvement 2\.5000/)).toBeInTheDocument();
    expect(screen.getByRole('img', { name: 'Rust evaluator 曲线与 replay' })).toBeInTheDocument();
  });

  it('uses the same selected scenario for metrics and the replay chart', async () => {
    const report = sentinelEvaluationReport();
    const staleScenario = structuredClone(report.scenarios[0]);
    staleScenario.id = 'stale_hold_short';
    staleScenario.title = 'SENTINEL stale short';
    staleScenario.direction = 'short';
    staleScenario.replayRows[0].strategyPrice = 108;
    staleScenario.replayRows[0].simulatedPositionExposure = -4.5;
    report.scenarios.push(staleScenario);
    const bridge = createMockBridge({
      isTauriEnvironment: () => true,
      evaluateTrack: vi.fn(async () => report),
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-sentinel',
        drafts: [makeDraft('draft-sentinel')],
        temporaryPriceOverrides: {},
      },
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );
    fireEvent.change(screen.getByLabelText('评估标的 fixture'), {
      target: { value: 'base_asset_v1' },
    });
    await screen.findByRole('heading', { name: 'SENTINEL 价格优势 long' });
    const firstReplayPath = document.querySelector('.track-workbench-chart__risk-line')
      ?.getAttribute('d');

    fireEvent.change(screen.getByLabelText('评估场景'), {
      target: { value: 'stale_hold_short' },
    });
    expect(await screen.findByRole('heading', { name: 'SENTINEL stale short' })).toBeInTheDocument();
    await waitFor(() => {
      expect(document.querySelector('.track-workbench-chart__risk-line')?.getAttribute('d'))
        .not.toBe(firstReplayPath);
    });
  });

  it('requires an explicit fixture and re-evaluates when the local selector changes', async () => {
    const bridge = createMockBridge({ isTauriEnvironment: () => true });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-sentinel',
        drafts: [makeDraft('draft-sentinel')],
        temporaryPriceOverrides: {},
      },
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );
    expect(screen.getByLabelText('评估标的 fixture')).toHaveValue('');
    expect(bridge.evaluateTrack).not.toHaveBeenCalled();

    fireEvent.change(screen.getByLabelText('评估标的 fixture'), {
      target: { value: 'base_asset_v1' },
    });
    await waitFor(() => expect(bridge.evaluateTrack).toHaveBeenCalledTimes(1));

    fireEvent.change(screen.getByLabelText('评估标的 fixture'), {
      target: { value: 'inverse_contract_v1' },
    });
    await waitFor(() => expect(bridge.evaluateTrack).toHaveBeenLastCalledWith(
      expect.objectContaining({ instrumentFixture: 'inverse_contract_v1' }),
    ));
    expect(store.getState().drafts[0].attachments).not.toHaveProperty('instrumentFixture');
  });

  it('shows evaluator loading and error states without retaining an old report', async () => {
    let rejectEvaluation!: (reason: unknown) => void;
    const pending = new Promise<WorkbenchEvaluationReport>((_resolve, reject) => {
      rejectEvaluation = reject;
    });
    const bridge = createMockBridge({
      isTauriEnvironment: () => true,
      evaluateTrack: vi.fn(() => pending),
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-sentinel',
        drafts: [makeDraft('draft-sentinel')],
        temporaryPriceOverrides: {},
      },
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    fireEvent.change(screen.getByLabelText('评估标的 fixture'), {
      target: { value: 'base_asset_v1' },
    });
    expect(await screen.findByText('Rust evaluator 正在运行…')).toBeInTheDocument();
    await act(async () => rejectEvaluation(new Error('EVALUATION_SENTINEL_ERROR')));
    expect(await screen.findByText('EVALUATION_SENTINEL_ERROR')).toBeInTheDocument();
    expect(screen.queryByText(/RULES_SENTINEL/)).toBeNull();
  });

  it('states that browser preview has no Rust evaluator or TypeScript fallback', async () => {
    await renderShell();

    expect(screen.getByText(/Rust evaluator 不可用/)).toBeInTheDocument();
    expect(screen.getByText(/不会运行 TypeScript 计算回退/)).toBeInTheDocument();
  });

  it('keeps analysis panels and the editor in separate workspace rails', async () => {
    await renderShell();

    const analysisRail = document.querySelector('.app-shell__analysis');
    const editorRail = document.querySelector('.app-shell__editor-rail');

    expect(analysisRail).not.toBeNull();
    expect(editorRail).not.toBeNull();
    expect(
      analysisRail?.querySelector('[aria-label="关键指标区"]'),
    ).not.toBeNull();
    expect(
      analysisRail?.querySelector('[aria-label="主图区"]'),
    ).not.toBeNull();
    expect(
      editorRail?.querySelector('[aria-label="参数编辑区"]'),
    ).not.toBeNull();
  });

  it('uses compact metric cards and avoids three-column editor grids', async () => {
    await renderShell();

    const metricRegion = screen.getByRole('region', { name: '关键指标区' });
    const editorRegion = screen.getByRole('region', { name: '参数编辑区' });

    expect(metricRegion.querySelector('.evaluation-controls')).not.toBeNull();
    expect(editorRegion.querySelector('.field-grid--three')).toBeNull();
    expect(editorRegion.querySelector('.field-grid--two')).not.toBeNull();
  });

  it('keeps section eyebrow labels but removes large editor section titles', async () => {
    await renderShell();

    const editorRegion = screen.getByRole('region', { name: '参数编辑区' });

    expect(editorRegion.querySelector('.editor-section__header')).not.toBeNull();
    expect(within(editorRegion).getByText('标识')).toBeInTheDocument();
    expect(within(editorRegion).getByText('价格带')).toBeInTheDocument();
    expect(within(editorRegion).getByText('仓位与调仓')).toBeInTheDocument();
    expect(within(editorRegion).getByText('止损与带外策略')).toBeInTheDocument();
    expect(within(editorRegion).getByText('曲线与预览')).toBeInTheDocument();
    expect(within(editorRegion).queryByText('Track 基本信息')).toBeNull();
    expect(within(editorRegion).queryByText('带宽与边界')).toBeNull();
    expect(within(editorRegion).queryByText('容量、名义与最小步长')).toBeNull();
    expect(within(editorRegion).queryByText('风险预算')).toBeNull();
    expect(within(editorRegion).queryByText('曲线家族与试算锚点')).toBeNull();
  });

  it('shows an undo notice after deleting a track', async () => {
    await renderShell();

    fireEvent.click(screen.getByRole('button', { name: '删除 Track silver' }));

    expect(screen.getByText('已删除 silver，可撤销')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '撤销' })).toBeEnabled();
  });

  it('shows immediate validation feedback for invalid risk input without blocking the edit', async () => {
    await renderShell();

    const input = screen.getByLabelText('日内止损');
    fireEvent.change(input, { target: { value: '0' } });

    expect(screen.getByText('daily_loss_limit 必须大于 0')).toBeInTheDocument();
    expect(screen.getByDisplayValue('0')).toBeInTheDocument();
    expect(screen.queryByRole('img', { name: 'Rust evaluator 曲线与 replay' })).toBeNull();
  });

  it('edits risk acquisition settings in the selected draft', async () => {
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-silver',
        drafts: [
          makeDraft('draft-silver', {
            riskAcquisition: {
              initialRatio: '0.5',
              advantageSteps: '2',
              minReleaseSteps: '1',
              maxReleaseSteps: '4',
              catchupRatio: '0.25',
              staleReleaseMinutes: '60',
            },
          }),
        ],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/metals.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell />
      </WorkbenchStoreProvider>,
    );

    const editorRegion = screen.getByRole('region', { name: '参数编辑区' });
    expect(within(editorRegion).getByText('风险暴露获取')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('优势倍数'), {
      target: { value: '3' },
    });

    expect(store.getState().drafts[0].riskAcquisition.advantageSteps).toBe('3');
  });

  it('uses bridge risk acquisition defaults when creating a blank draft', async () => {
    const bridge = createMockBridge({
      isTauriEnvironment: () => true,
      loadRiskAcquisitionDefaults: vi.fn(async () => ({
        initialRatio: '0.4',
        advantageSteps: '1.5',
        minReleaseSteps: '0.75',
        maxReleaseSteps: '3',
        catchupRatio: '0.2',
        staleReleaseMinutes: '20',
      })),
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-silver',
        drafts: [makeDraft('draft-silver')],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/metals.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    await waitFor(() => {
      expect(bridge.loadRiskAcquisitionDefaults).toHaveBeenCalledTimes(1);
    });

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '空白新建' }));
    });

    const createdDraft = store.getState().drafts.at(-1);
    expect(createdDraft?.riskAcquisition).toEqual({
      initialRatio: '0.4',
      advantageSteps: '1.5',
      minReleaseSteps: '0.75',
      maxReleaseSteps: '3',
      catchupRatio: '0.2',
      staleReleaseMinutes: '20',
    });
  });

  it('loads Rust defaults before enabling blank creation in an empty workspace', async () => {
    let resolveDefaults!: (value: TrackDraft['riskAcquisition']) => void;
    const pendingDefaults = new Promise<TrackDraft['riskAcquisition']>((resolve) => {
      resolveDefaults = resolve;
    });
    const bridge = createMockBridge({
      isTauriEnvironment: () => true,
      loadRiskAcquisitionDefaults: vi.fn(() => pendingDefaults),
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: '',
        drafts: [],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      render(
        <WorkbenchStoreProvider store={store}>
          <AppShell bridge={bridge} />
        </WorkbenchStoreProvider>,
      );
    });

    expect(bridge.loadRiskAcquisitionDefaults).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('button', { name: '空白新建' })).toBeDisabled();

    await act(async () => {
      resolveDefaults({
        initialRatio: '0.41',
        advantageSteps: '1.6',
        minReleaseSteps: '0.8',
        maxReleaseSteps: '3.2',
        catchupRatio: '0.21',
        staleReleaseMinutes: '21',
      });
      await pendingDefaults;
    });
    await waitFor(() => expect(screen.getByRole('button', { name: '空白新建' })).toBeEnabled());
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '空白新建' }));
    });

    expect(store.getState().drafts[0]?.riskAcquisition.initialRatio).toBe('0.41');
  });

  it('copies current track through the export bridge and only writes [[tracks]] text', async () => {
    const bridge = createMockBridge();
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-silver',
        drafts: [makeDraft('draft-silver')],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/metals.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: '复制当前 Track' }));
    });

    expect(bridge.exportCurrentTrack).toHaveBeenCalledTimes(1);
    expect(bridge.copyText).toHaveBeenCalledWith('[[tracks]]\ntrack_id = "silver"');
    expect(screen.getByText('当前 Track 已复制到剪贴板')).toBeInTheDocument();
  });

  it('shows a readable quote error when Binance does not support the symbol', async () => {
    const bridge = createMockBridge({
      fetchBinanceQuote: vi.fn(async () => ({
        price: null,
        retrievedAt: 1_713_400_000_000,
        errorKind: 'unsupported_symbol' as const,
        errorMessage: 'Binance 合约不支持 symbol `BADPAIR`',
      })),
    });

    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-bad',
        drafts: [
          makeDraft('draft-bad', {
            additional: {
              trackId: 'bad',
              symbol: 'BADPAIR',
            },
            ui: {
              quotePriceInput: '',
            },
          }),
        ],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/bad.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    expect((await screen.findAllByText('Binance 合约不支持 symbol `BADPAIR`')).length)
      .toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText('symbol 不支持').length).toBeGreaterThanOrEqual(1);
  });

  it('passes the loaded exchange venue when refreshing remote quotes', async () => {
    const bridge = createMockBridge();
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-okx',
        drafts: [
          makeDraft('draft-okx', {
            additional: {
              trackId: 'okx-eth',
              symbol: 'ETH-USDT-SWAP',
            },
            ui: {
              quotePriceInput: '',
            },
            attachments: {
              exchangeVenue: 'okx',
            },
          }),
        ],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/okx.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    await waitFor(() => {
      expect(bridge.fetchBinanceQuote).toHaveBeenCalledWith({
        exchangeVenue: 'okx',
        symbol: 'ETH-USDT-SWAP',
      });
    });
  });

  it('describes the temporary price override using the selected exchange venue', async () => {
    const draft = makeDraft('draft-okx', {
      additional: {
        trackId: 'okx-anthropic',
        symbol: 'ANTHROPIC-USDT-SWAP',
      },
      ui: {
        quotePriceInput: '',
      },
      attachments: {
        exchangeVenue: 'okx',
      },
    });
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: draft.draftId,
        drafts: [draft],
        temporaryPriceOverrides: {},
      },
    });

    await act(async () => {
      await store.load('/tmp/strategies/okx.toml', store.getState());
    });

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell />
      </WorkbenchStoreProvider>,
    );

    expect(screen.getByText(/默认使用 OKX 合约实时价格/)).toBeInTheDocument();
    expect(screen.queryByText(/默认使用 Binance 合约实时价格/)).toBeNull();
  });
});
