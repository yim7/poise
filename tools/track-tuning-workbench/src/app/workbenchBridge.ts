import type { SessionPersistence } from '@/state/sessionSync';
import type {
  RemoteQuoteErrorKind,
  WorkbenchSnapshot,
} from '@/state/workbenchStore';
import { createTrackDraft, type TrackDraft } from '@/domain/trackDraft';

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
  configPath: string;
  projectedTracks: Array<{
    draftId: string;
    fields: {
      trackId: string;
      symbol: string;
      lowerPrice: number;
      upperPrice: number;
      longExposureUnits: number;
      shortExposureUnits: number;
      notionalPerUnit: number;
      maxNotional: number;
      minRebalanceUnits: number;
      leverage: number;
      outOfBandPolicy: string;
      dailyLossLimit: number;
      totalLossLimit: number;
      shapeFamily: string;
    };
  }>;
}

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
): SessionPersistence {
  return {
    loadDraft(configPath) {
      return bridge.loadSavedDraft(configPath);
    },
    saveDraft(configPath, snapshot) {
      return bridge.saveDraft(configPath, snapshot);
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
      return tauriInvoke<string | null>('open_config_file');
    },
    async loadConfigFile(configPath) {
      const payload = await tauriInvoke<LoadedConfigFilePayload>('load_config_file', {
        configPath,
      });
      return {
        configPath: payload.configPath,
        projectedTracks: payload.projectedTracks.map((track) => createTrackDraft({
          draftId: track.draftId,
          raw: {
            trackId: track.fields.trackId,
            symbol: track.fields.symbol,
            lowerPrice: formatRawNumber(track.fields.lowerPrice),
            upperPrice: formatRawNumber(track.fields.upperPrice),
            longExposureUnits: formatRawNumber(track.fields.longExposureUnits),
            shortExposureUnits: formatRawNumber(track.fields.shortExposureUnits),
            notionalPerUnit: formatRawNumber(track.fields.notionalPerUnit),
            maxNotional: formatRawNumber(track.fields.maxNotional),
            minRebalanceUnits: formatRawNumber(track.fields.minRebalanceUnits),
            leverage: String(track.fields.leverage),
            dailyLossLimit: formatRawNumber(track.fields.dailyLossLimit),
            totalLossLimit: formatRawNumber(track.fields.totalLossLimit),
            outOfBandPolicy: track.fields.outOfBandPolicy as TrackDraft['enums']['outOfBandPolicy'],
            shapeFamily: track.fields.shapeFamily as TrackDraft['enums']['shapeFamily'],
          },
          ui: {
            quotePriceInput: '',
          },
        })),
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
    draftId: draft.draftId,
    fields: {
      trackId: draft.additional.trackId,
      symbol: draft.additional.symbol,
      lowerPrice: parseRequiredNumber(draft.rawNumbers.lowerPrice),
      upperPrice: parseRequiredNumber(draft.rawNumbers.upperPrice),
      longExposureUnits: parseRequiredNumber(draft.rawNumbers.longExposureUnits),
      shortExposureUnits: parseRequiredNumber(draft.rawNumbers.shortExposureUnits),
      notionalPerUnit: parseRequiredNumber(draft.rawNumbers.notionalPerUnit),
      maxNotional: parseRequiredNumber(draft.rawNumbers.maxNotional),
      minRebalanceUnits: parseRequiredNumber(draft.rawNumbers.minRebalanceUnits),
      leverage: Math.trunc(parseRequiredNumber(draft.rawNumbers.leverage)),
      outOfBandPolicy: draft.enums.outOfBandPolicy,
      dailyLossLimit: parseRequiredNumber(draft.rawNumbers.dailyLossLimit),
      totalLossLimit: parseRequiredNumber(draft.rawNumbers.totalLossLimit),
      shapeFamily: draft.enums.shapeFamily,
    },
  };
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
