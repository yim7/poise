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
  type TrackDraft,
  type TrackDraftFieldKey,
  type TrackDraftLoadIssue,
} from '@/domain/trackDraft';
import { withBinanceFuturesDefaults } from '@/domain/binanceFuturesDefaults';

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

export interface WorkbenchBridge {
  isTauriEnvironment(): boolean;
  openConfigFile(): Promise<string | null>;
  loadConfigFile(configPath: string): Promise<WorkbenchBridgeLoadedConfig>;
  loadSavedDraft(configPath: string): Promise<WorkbenchSnapshot | null>;
  saveDraft(configPath: string, snapshot: WorkbenchSnapshot): Promise<void>;
  exportCurrentTrack(draft: TrackDraft): Promise<string>;
  exportAllTracks(drafts: TrackDraft[]): Promise<string>;
  copyText(text: string): Promise<void>;
  fetchBinanceQuote(symbol: string): Promise<WorkbenchBridgeQuotePayload>;
}

interface LoadedConfigFilePayload {
  config_path: string;
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
    };
    load_issues: Array<{
      field_key: string;
      message: string;
    }>;
  }>;
}

type BandProtectionPolicyPayload =
  | { freeze: Record<string, never> }
  | { flatten: { trigger_bps: number; recover: 'back_in_band' | { price_confirm: { bps: number } } } }
  | { terminate: Record<string, never> };

interface TauriQuotePayload {
  price: string | null;
  retrieved_at: number;
  error_kind: RemoteQuoteErrorKind | null;
  error_message: string | null;
}

interface BrowserQuoteErrorBody {
  code?: number;
  msg?: string;
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
      return {
        configPath: payload.config_path,
        projectedTracks: payload.projected_tracks.map((track) =>
          withBinanceFuturesDefaults(
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
                bandProtectionKind: bandProtectionKindFromPayload(track.fields.out_of_band_policy),
                shapeFamily: track.fields.shape_family as TrackDraft['enums']['shapeFamily'],
              },
              ui: {
                quotePriceInput: '',
              },
              attachments: track.load_issues.length > 0
                ? {
                    loadIssues: track.load_issues.map(normalizeLoadIssue),
                  }
                : undefined,
            }),
          ),
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
    async fetchBinanceQuote(symbol) {
      const payload = await tauriInvoke<TauriQuotePayload>('fetch_binance_quote', {
        symbol,
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
      throw new Error('浏览器开发模式不支持直接读取外部配置文件，请在 Tauri 桌面应用中使用。');
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
    async fetchBinanceQuote(symbol) {
      const normalizedSymbol = symbol.trim().toUpperCase();
      if (!normalizedSymbol) {
        return {
          price: null,
          retrievedAt: Date.now(),
          errorKind: 'unsupported_symbol',
          errorMessage: 'symbol 不能为空',
        };
      }

      try {
        const response = await fetch(
          `https://fapi.binance.com/fapi/v1/ticker/price?symbol=${encodeURIComponent(normalizedSymbol)}`,
        );
        const retrievedAt = Date.now();
        const body = (await response.json().catch(() => null)) as
          | { price?: string }
          | BrowserQuoteErrorBody
          | null;

        if (!response.ok) {
          return {
            price: null,
            retrievedAt,
            errorKind: classifyBrowserQuoteError(response.status, body),
            errorMessage: buildBrowserQuoteErrorMessage(response.status, normalizedSymbol, body),
          };
        }

        if (!hasBrowserQuotePrice(body)) {
          return {
            price: null,
            retrievedAt,
            errorKind: 'invalid_response',
            errorMessage: '解析 Binance 合约报价失败',
          };
        }

        return {
          price: body.price,
          retrievedAt,
          errorKind: null,
          errorMessage: null,
        };
      } catch (error) {
        return {
          price: null,
          retrievedAt: Date.now(),
          errorKind: 'network',
          errorMessage: `请求 Binance 合约报价失败: ${String(error)}`,
        };
      }
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
      out_of_band_policy: toBandProtectionPolicyPayload(draft.enums.bandProtectionKind),
      daily_loss_limit: parseRequiredNumber(draft.rawNumbers.dailyLossLimit),
      total_loss_limit: parseRequiredNumber(draft.rawNumbers.totalLossLimit),
      shape_family: draft.enums.shapeFamily,
    },
    load_issues: [],
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
    default:
      return 'trackId';
  }
}

function bandProtectionKindFromPayload(
  policy: BandProtectionPolicyPayload,
): TrackDraft['enums']['bandProtectionKind'] {
  if ('freeze' in policy) {
    return 'freeze';
  }
  if ('flatten' in policy) {
    return 'flatten';
  }
  return 'terminate';
}

function toBandProtectionPolicyPayload(
  kind: TrackDraft['enums']['bandProtectionKind'],
): BandProtectionPolicyPayload {
  switch (kind) {
    case 'freeze':
      return { freeze: {} };
    case 'flatten':
      return {
        flatten: {
          trigger_bps: 500,
          recover: {
            price_confirm: { bps: 500 },
          },
        },
      };
    case 'terminate':
      return { terminate: {} };
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

function classifyBrowserQuoteError(
  status: number,
  body: BrowserQuoteErrorBody | { price?: string } | null,
): RemoteQuoteErrorKind {
  if (status === 400 && body && 'code' in body && body.code === -1121) {
    return 'unsupported_symbol';
  }
  if (status === 429 || status === 418 || (body && 'code' in body && body.code === -1003)) {
    return 'rate_limited';
  }
  if (status === 503) {
    return 'temporarily_unavailable';
  }
  return 'upstream';
}

function buildBrowserQuoteErrorMessage(
  status: number,
  symbol: string,
  body: BrowserQuoteErrorBody | { price?: string } | null,
) {
  const upstreamMessage =
    body && typeof body === 'object' && 'msg' in body && typeof body.msg === 'string'
      ? body.msg
      : 'unknown error';

  if (status === 400 && body && 'code' in body && body.code === -1121) {
    return `Binance 合约不支持 symbol \`${symbol}\`: ${upstreamMessage}`;
  }
  if (status === 429 || status === 418 || (body && 'code' in body && body.code === -1003)) {
    return `Binance 合约限流中，请稍后重试: ${upstreamMessage}`;
  }
  if (status === 503) {
    return `Binance 合约暂时不可用: ${upstreamMessage}`;
  }
  return `Binance 合约报价请求失败 (${status}): ${upstreamMessage}`;
}

function hasBrowserQuotePrice(
  body: BrowserQuoteErrorBody | { price?: string } | null,
): body is { price: string } {
  return Boolean(body && typeof body === 'object' && 'price' in body && typeof body.price === 'string');
}
