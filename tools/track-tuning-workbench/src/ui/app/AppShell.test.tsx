import '@testing-library/jest-dom/vitest';

import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { AppShell } from '@/app/AppShell';
import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';
import type { WorkbenchBridge } from '@/app/workbenchBridge';

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
    riskAcquisition: overrides.riskAcquisition,
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
          exchangeRules: {
            priceTick: 0.1,
            quantityStep: 0.01,
            takerFeeRate: 0.0005,
          },
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
      initialRatio: '0.3',
      advantageSteps: '2',
      minReleaseSteps: '1',
      maxReleaseSteps: '4',
      catchupRatio: '0.25',
      staleReleaseMinutes: '30',
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
    ...overrides,
  };
}

describe('AppShell', () => {
  it('renders the primary actions and metric cards for the selected track', async () => {
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
    expect(within(metricRegion).getByText('当前价格')).toBeInTheDocument();
    expect(within(metricRegion).getByText('最小步长对应价格')).toBeInTheDocument();
    expect(within(metricRegion).getByText('每步理论净利')).toBeInTheDocument();
    expect(within(metricRegion).getByText('当前价到风险边缘')).toBeInTheDocument();
    expect(within(metricRegion).getByText('从 0 建仓到边缘均价')).toBeInTheDocument();
    expect(within(metricRegion).getByText('从 0 到边缘理论浮亏')).toBeInTheDocument();
    expect(within(metricRegion).getByText('从 0 到边缘净亏估算')).toBeInTheDocument();
  });

  it('keeps server live disabled outside the Tauri bridge', async () => {
    await renderShell();

    const serverPanel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(serverPanel).getByText('浏览器预览模式')).toBeInTheDocument();
    expect(within(serverPanel).getByText('server live 只在 Tauri 桌面版启用。')).toBeInTheDocument();
  });

  it('enables server live when AppShell receives a Tauri bridge', () => {
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

    render(
      <WorkbenchStoreProvider store={store}>
        <AppShell bridge={bridge} />
      </WorkbenchStoreProvider>,
    );

    const serverPanel = screen.getByRole('region', { name: 'Server live inspector' });
    expect(within(serverPanel).queryByText('浏览器预览模式')).toBeNull();
    expect(within(serverPanel).getByRole('button', { name: '连接 Server' })).toBeEnabled();
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

    expect(metricRegion.querySelector('.metric-cards--compact')).not.toBeNull();
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
    expect(screen.getByRole('img', { name: 'Track 调参主图' })).toBeInTheDocument();
    expect(
      within(screen.getByRole('region', { name: '关键指标区' })).getByText('当前价格'),
    ).toBeInTheDocument();
  });

  it('edits risk acquisition settings in the selected draft', async () => {
    const store = createWorkbenchStore({
      initialSnapshot: {
        selectedDraftId: 'draft-silver',
        drafts: [
          makeDraft('draft-silver', {
            riskAcquisition: {
              initialRatio: '0.3',
              advantageSteps: '2',
              minReleaseSteps: '1',
              maxReleaseSteps: '4',
              catchupRatio: '0.25',
              staleReleaseMinutes: '30',
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

  it('recomputes min rebalance metrics when the min rebalance units field changes', async () => {
    await renderShell();

    const metricRegion = screen.getByRole('region', { name: '关键指标区' });
    expect(within(metricRegion).getByText('最小位移 0.63 / 0.63')).toBeInTheDocument();
    expect(within(metricRegion).getByText('下 1.04 / 上 1.04')).toBeInTheDocument();
    expect(within(metricRegion).getByText('毛利 1.17 / 1.17 · 费 0.13 / 0.13')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('最小调仓单位'), {
      target: { value: '1.0' },
    });

    expect(screen.getByDisplayValue('1.0')).toBeInTheDocument();
    expect(within(metricRegion).getByText('最小位移 1.25 / 1.25')).toBeInTheDocument();
    expect(within(metricRegion).getByText('下 98.75 / 上 101.25')).toBeInTheDocument();
    expect(within(metricRegion).getByText('下 4.43 / 上 4.42')).toBeInTheDocument();
    expect(within(metricRegion).getByText('毛利 4.69 / 4.69 · 费 0.26 / 0.26')).toBeInTheDocument();
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

    expect(await screen.findByText('Binance 合约不支持 symbol `BADPAIR`')).toBeInTheDocument();
    expect(screen.getByText('symbol 不支持')).toBeInTheDocument();
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
