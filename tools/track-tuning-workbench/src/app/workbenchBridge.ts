import type {
  BrowserStorageLike,
  SessionPersistence,
} from '@/state/sessionSync';
import { createBrowserSessionPersistence } from '@/state/sessionSync';
import type {
  RemoteQuoteErrorKind,
  WorkbenchSnapshot,
} from '@/state/workbenchStore';
import {
  createTrackDraft,
  type BandProtectionPolicyPayload,
  type RiskIncreaseDelayDraft,
  type TrackDraft,
  type TrackDraftFieldKey,
  type TrackDraftLoadIssue,
} from '@/domain/trackDraft';

export interface WorkbenchBridgeCommandError {
  kind: 'config' | 'io' | 'session_store' | 'dialog' | 'clipboard' | 'internal';
  message: string;
}

export interface WorkbenchBridgeLoadedConfig {
  configPath: string;
  projectedTracks: TrackDraft[];
}

export interface WorkbenchBridgeQuotePayload {
  price: string | null;
  retrievedAt: number;
  errorKind: RemoteQuoteErrorKind | null;
  errorMessage: string | null;
}

export interface WorkbenchBridgeQuoteRequest {
  symbol: string;
  exchangeVenue?: string | null;
}

export interface WorkbenchBridge {
  isTauriEnvironment(): boolean;
  openConfigFile(): Promise<string | null>;
  loadConfigFile(configPath: string): Promise<WorkbenchBridgeLoadedConfig>;
  loadSavedDraft(configPath: string): Promise<WorkbenchSnapshot | null>;
  saveDraft(configPath: string, snapshot: WorkbenchSnapshot): Promise<void>;
  exportCurrentTrack(draft: TrackDraft): Promise<string>;
  exportAllTracks(drafts: TrackDraft[]): Promise<string>;
  copyText(text: string): Promise<void>;
  fetchBinanceQuote(request: WorkbenchBridgeQuoteRequest): Promise<WorkbenchBridgeQuotePayload>;
}

interface LoadedConfigFilePayload {
  config_path: string;
  exchange_venue?: string;
  projected_tracks: Array<{
    draft_id: string;
    fields: {
      track_id: string;
      symbol: string;
      lower_price: number;
      upper_price: number;
      long_exposure_units: number;
      short_exposure_units: number;
      notional_per_unit: number;
      max_notional: number;
      min_rebalance_units: number;
      leverage: number;
      out_of_band_policy: BandProtectionPolicyPayload;
      daily_loss_limit: number;
      total_loss_limit: number;
      shape_family: string;
      risk_increase_delay?: {
        startup_initial_ratio: number;
        advantage_min_rebalance_multiples: number;
        base_step_min_rebalance_multiples: number;
        max_step_min_rebalance_multiples: number;
        catchup_ratio: number;
      } | null;
    };
    load_issues: Array<{
      field_key: string;
      message: string;
    }>;
  }>;
}

interface TauriQuotePayload {
  price: string | null;
  retrieved_at: number;
  error_kind: RemoteQuoteErrorKind | null;
  error_message: string | null;
}

export function createWorkbenchBridge(): WorkbenchBridge {
  return isTauriEnvironment() ? createTauriWorkbenchBridge() : createBrowserWorkbenchBridge();
}

export function createBridgeSessionPersistence(
  bridge: Pick<WorkbenchBridge, 'loadSavedDraft' | 'saveDraft'>,
  storage?: BrowserStorageLike,
): SessionPersistence {
  const browserMirror = storage
    ? createBrowserSessionPersistence(storage, {
        namespace: 'poise.track-tuning-workbench.tauri',
      })
    : null;

  return {
    async loadDraft(configPath) {
      const mirrored = browserMirror ? await browserMirror.loadDraft(configPath) : null;
      if (mirrored) {
        return mirrored;
      }

      const snapshot = await bridge.loadSavedDraft(configPath);
      if (snapshot && browserMirror?.saveDraftSync) {
        browserMirror.saveDraftSync(configPath, snapshot);
      }
      return snapshot;
    },
    async saveDraft(configPath, snapshot) {
      browserMirror?.saveDraftSync?.(configPath, snapshot);
      await bridge.saveDraft(configPath, snapshot);
    },
    saveDraftSync(configPath, snapshot) {
      browserMirror?.saveDraftSync?.(configPath, snapshot);
    },
  };
}

export function createSourceSnapshot(
  config: WorkbenchBridgeLoadedConfig,
): WorkbenchSnapshot {
  return {
    selectedDraftId: config.projectedTracks[0]?.draftId ?? '',
    drafts: config.projectedTracks.map((draft) => structuredClone(draft)),
    temporaryPriceOverrides: {},
  };
}

function createTauriWorkbenchBridge(): WorkbenchBridge {
  return {
    isTauriEnvironment,
    async openConfigFile() {
      const { open } = await import('@tauri-apps/plugin-dialog');
      const selection = await open({
        title: '选择 Track 配置文件',
        directory: false,
        multiple: false,
        filters: [
          {
            name: 'TOML',
            extensions: ['toml'],
          },
        ],
      });

      if (Array.isArray(selection)) {
        return selection[0] ?? null;
      }

      return selection;
    },
    async loadConfigFile(configPath) {
      const payload = await tauriInvoke<LoadedConfigFilePayload>('load_config_file', {
        configPath,
      });
      const exchangeVenue = payload.exchange_venue ?? 'binance';
      return {
        configPath: payload.config_path,
        projectedTracks: payload.projected_tracks.map((track) =>
          createTrackDraft({
            draftId: track.draft_id,
            raw: {
              trackId: track.fields.track_id,
              symbol: track.fields.symbol,
              lowerPrice: formatRawNumber(track.fields.lower_price),
              upperPrice: formatRawNumber(track.fields.upper_price),
              longExposureUnits: formatRawNumber(track.fields.long_exposure_units),
              shortExposureUnits: formatRawNumber(track.fields.short_exposure_units),
              notionalPerUnit: formatRawNumber(track.fields.notional_per_unit),
              maxNotional: formatRawNumber(track.fields.max_notional),
              minRebalanceUnits: formatRawNumber(track.fields.min_rebalance_units),
              leverage: String(track.fields.leverage),
              dailyLossLimit: formatRawNumber(track.fields.daily_loss_limit),
              totalLossLimit: formatRawNumber(track.fields.total_loss_limit),
              bandProtectionPolicy: track.fields.out_of_band_policy,
              shapeFamily: track.fields.shape_family as TrackDraft['enums']['shapeFamily'],
            },
            ui: {
              quotePriceInput: '',
            },
            riskIncreaseDelay: track.fields.risk_increase_delay
              ? {
                  startupInitialRatio: formatRawNumber(
                    track.fields.risk_increase_delay.startup_initial_ratio,
                  ),
                  advantageMinRebalanceMultiples: formatRawNumber(
                    track.fields.risk_increase_delay.advantage_min_rebalance_multiples,
                  ),
                  baseStepMinRebalanceMultiples: formatRawNumber(
                    track.fields.risk_increase_delay.base_step_min_rebalance_multiples,
                  ),
                  maxStepMinRebalanceMultiples: formatRawNumber(
                    track.fields.risk_increase_delay.max_step_min_rebalance_multiples,
                  ),
                  catchupRatio: formatRawNumber(
                    track.fields.risk_increase_delay.catchup_ratio,
                  ),
                }
              : undefined,
            attachments: {
              exchangeVenue,
              ...(track.load_issues.length > 0
                ? { loadIssues: track.load_issues.map(normalizeLoadIssue) }
                : {}),
            },
          }),
        ),
      };
    },
    async loadSavedDraft(configPath) {
      return tauriInvoke<WorkbenchSnapshot | null>('load_saved_draft', { configPath });
    },
    async saveDraft(configPath, snapshot) {
      await tauriInvoke('save_draft', { configPath, draftSnapshot: snapshot });
    },
    async exportCurrentTrack(draft) {
      return tauriInvoke<string>('export_current_track', {
        draft: toTrackDraftPayload(draft),
      });
    },
    async exportAllTracks(drafts) {
      return tauriInvoke<string>('export_all_tracks', {
        drafts: drafts.map(toTrackDraftPayload),
      });
    },
    async copyText(text) {
      await tauriInvoke('copy_text', { text });
    },
    async fetchBinanceQuote(request) {
      const payload = await tauriInvoke<TauriQuotePayload>('fetch_binance_quote', {
        symbol: request.symbol,
        exchangeVenue: request.exchangeVenue ?? null,
      });
      return {
        price: payload.price,
        retrievedAt: payload.retrieved_at,
        errorKind: payload.error_kind,
        errorMessage: payload.error_message,
      };
    },
  };
}

function createBrowserWorkbenchBridge(): WorkbenchBridge {
  return {
    isTauriEnvironment: () => false,
    async openConfigFile() {
      return null;
    },
    async loadConfigFile() {
      throw new Error('浏览器预览不读取真实配置文件，请在 Tauri 桌面应用中使用。');
    },
    async loadSavedDraft() {
      return null;
    },
    async saveDraft() {},
    async exportCurrentTrack() {
      throw new Error('浏览器开发模式不支持导出命令，请在 Tauri 桌面应用中使用。');
    },
    async exportAllTracks() {
      throw new Error('浏览器开发模式不支持导出命令，请在 Tauri 桌面应用中使用。');
    },
    async copyText(text) {
      if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
        return;
      }
      throw new Error('当前环境不支持剪贴板写入。');
    },
    async fetchBinanceQuote() {
      return {
        price: null,
        retrievedAt: Date.now(),
        errorKind: 'temporarily_unavailable',
        errorMessage: '浏览器预览不连接交易所报价，请在 Tauri 桌面应用中使用实时数据。',
      };
    },
  };
}

function isTauriEnvironment() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

async function tauriInvoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');

  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw normalizeWorkbenchBridgeError(error);
  }
}

function normalizeWorkbenchBridgeError(error: unknown): Error {
  if (isCommandError(error)) {
    return new Error(error.message);
  }

  if (error instanceof Error) {
    return error;
  }

  return new Error(String(error));
}

function isCommandError(error: unknown): error is WorkbenchBridgeCommandError {
  return (
    typeof error === 'object'
    && error !== null
    && 'message' in error
    && typeof (error as { message?: unknown }).message === 'string'
  );
}

function toTrackDraftPayload(draft: TrackDraft) {
  return {
    draft_id: draft.draftId,
    fields: {
      track_id: draft.additional.trackId,
      symbol: draft.additional.symbol,
      lower_price: parseRequiredNumber(draft.rawNumbers.lowerPrice),
      upper_price: parseRequiredNumber(draft.rawNumbers.upperPrice),
      long_exposure_units: parseRequiredNumber(draft.rawNumbers.longExposureUnits),
      short_exposure_units: parseRequiredNumber(draft.rawNumbers.shortExposureUnits),
      notional_per_unit: parseRequiredNumber(draft.rawNumbers.notionalPerUnit),
      max_notional: parseRequiredNumber(draft.rawNumbers.maxNotional),
      min_rebalance_units: parseRequiredNumber(draft.rawNumbers.minRebalanceUnits),
      leverage: Math.trunc(parseRequiredNumber(draft.rawNumbers.leverage)),
      out_of_band_policy:
        draft.enums.bandProtectionPolicy,
      daily_loss_limit: parseRequiredNumber(draft.rawNumbers.dailyLossLimit),
      total_loss_limit: parseRequiredNumber(draft.rawNumbers.totalLossLimit),
      shape_family: draft.enums.shapeFamily,
      risk_increase_delay: toRiskIncreaseDelayPayload(draft.riskIncreaseDelay),
    },
    load_issues: [],
  };
}

function toRiskIncreaseDelayPayload(delay: RiskIncreaseDelayDraft | undefined) {
  if (!delay) {
    return null;
  }

  return {
    startup_initial_ratio: parseRequiredNumber(delay.startupInitialRatio),
    advantage_min_rebalance_multiples:
      parseRequiredNumber(delay.advantageMinRebalanceMultiples),
    base_step_min_rebalance_multiples:
      parseRequiredNumber(delay.baseStepMinRebalanceMultiples),
    max_step_min_rebalance_multiples:
      parseRequiredNumber(delay.maxStepMinRebalanceMultiples),
    catchup_ratio: parseRequiredNumber(delay.catchupRatio),
  };
}

function normalizeLoadIssue(issue: LoadedConfigFilePayload['projected_tracks'][number]['load_issues'][number]): TrackDraftLoadIssue {
  return {
    field: normalizeLoadIssueField(issue.field_key),
    message: issue.message,
  };
}

function normalizeLoadIssueField(fieldKey: string): TrackDraftFieldKey {
  switch (fieldKey) {
    case 'track_id':
      return 'trackId';
    case 'symbol':
      return 'symbol';
    case 'lower_price':
      return 'lowerPrice';
    case 'upper_price':
      return 'upperPrice';
    case 'long_exposure_units':
      return 'longExposureUnits';
    case 'short_exposure_units':
      return 'shortExposureUnits';
    case 'notional_per_unit':
      return 'notionalPerUnit';
    case 'max_notional':
      return 'maxNotional';
    case 'min_rebalance_units':
      return 'minRebalanceUnits';
    case 'leverage':
      return 'leverage';
    case 'daily_loss_limit':
      return 'dailyLossLimit';
    case 'total_loss_limit':
      return 'totalLossLimit';
    case 'shape_family':
      return 'shapeFamily';
    case 'out_of_band_policy':
      return 'bandProtectionKind';
    case 'risk_increase_delay':
      return 'riskIncreaseDelay.startupInitialRatio';
    case 'risk_increase_delay.startup_initial_ratio':
      return 'riskIncreaseDelay.startupInitialRatio';
    case 'risk_increase_delay.advantage_min_rebalance_multiples':
      return 'riskIncreaseDelay.advantageMinRebalanceMultiples';
    case 'risk_increase_delay.base_step_min_rebalance_multiples':
      return 'riskIncreaseDelay.baseStepMinRebalanceMultiples';
    case 'risk_increase_delay.max_step_min_rebalance_multiples':
      return 'riskIncreaseDelay.maxStepMinRebalanceMultiples';
    case 'risk_increase_delay.catchup_ratio':
      return 'riskIncreaseDelay.catchupRatio';
    default:
      return 'trackId';
  }
}

function parseRequiredNumber(input: string) {
  const value = Number(input.trim());
  if (!Number.isFinite(value)) {
    throw new Error(`字段值不是有限数字: ${input}`);
  }
  return value;
}

function formatRawNumber(value: number) {
  if (Number.isInteger(value)) {
    return String(value);
  }
  return String(value);
}
