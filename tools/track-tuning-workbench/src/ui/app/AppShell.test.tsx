import '@testing-library/jest-dom/vitest';

import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { AppShell } from '@/app/AppShell';
import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';
import type { WorkbenchBridge } from '@/app/workbenchBridge';

interface DraftOverrides {
  additional?: TrackDraft['additional'];
  enums?: TrackDraft['enums'];
  rawNumbers?: Partial<TrackDraft['rawNumbers']>;
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
      outOfBandPolicy: overrides.enums?.outOfBandPolicy ?? 'freeze',
    },
    additional: overrides.additional,
    enums: overrides.enums,
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
    expect(within(metricRegion).getByText('当前价到风险边缘')).toBeInTheDocument();
    expect(within(metricRegion).getByText('零仓目标点到风险边缘')).toBeInTheDocument();
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
});
