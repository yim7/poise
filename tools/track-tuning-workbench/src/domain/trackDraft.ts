import { ensureTrackDraftParsed } from '@/domain/trackValidation';

export type TrackShapeFamily = 'linear' | 'inertial' | 'responsive';
export type TrackOutOfBandPolicy = 'freeze' | 'hold' | 'flatten' | 'terminate';

export interface TrackDraftNumericFields {
  lowerPrice: number;
  upperPrice: number;
  longExposureUnits: number;
  shortExposureUnits: number;
  notionalPerUnit: number;
  maxNotional: number;
  minRebalanceUnits: number;
  leverage: number;
  dailyLossLimit: number;
  totalLossLimit: number;
}

export interface TrackDraftRawNumericFields {
  lowerPrice: string;
  upperPrice: string;
  longExposureUnits: string;
  shortExposureUnits: string;
  notionalPerUnit: string;
  maxNotional: string;
  minRebalanceUnits: string;
  leverage: string;
  dailyLossLimit: string;
  totalLossLimit: string;
}

export interface TrackDraftEnumFields {
  shapeFamily: TrackShapeFamily;
  outOfBandPolicy: TrackOutOfBandPolicy;
}

export interface TrackDraftAdditionalFields {
  trackId: string;
  symbol: string;
}

export interface TrackExchangeRulesDraft {
  priceTick?: number;
  quantityStep?: number;
  minQty?: number;
  minNotional?: number;
  makerFeeRate?: number;
  takerFeeRate?: number;
}

export interface TrackLossGuardDraft {
  netRealizedPnlToday: number;
  netRealizedPnlCumulative: number;
  unrealizedPnl: number;
}

export interface TrackDraftAttachments {
  currentExposure?: number;
  exchangeRules?: TrackExchangeRulesDraft;
  lossGuard?: TrackLossGuardDraft;
}

export interface TrackDraftUiState {
  quotePriceInput: string;
}

export interface TrackDraftResolvedUiState extends TrackDraftUiState {
  quotePrice: number;
}

export interface TrackDraft {
  draftId: string;
  additional: TrackDraftAdditionalFields;
  rawNumbers: TrackDraftRawNumericFields;
  parsedNumbers: Partial<TrackDraftNumericFields>;
  enums: TrackDraftEnumFields;
  ui: TrackDraftUiState;
  attachments: TrackDraftAttachments;
}

export interface TrackDraftParsedSnapshot {
  draftId: string;
  additional: TrackDraftAdditionalFields;
  parsedNumbers: TrackDraftNumericFields;
  enums: TrackDraftEnumFields;
  ui: TrackDraftResolvedUiState;
  attachments: TrackDraftAttachments;
}

export interface CreateTrackDraftInput {
  draftId: string;
  raw: TrackDraftAdditionalFields &
    TrackDraftRawNumericFields &
    TrackDraftEnumFields;
  parsedNumbers?: Partial<TrackDraftNumericFields>;
  enums?: TrackDraftEnumFields;
  additional?: TrackDraftAdditionalFields;
  ui?: Partial<TrackDraftUiState>;
  attachments?: TrackDraftAttachments;
}

export const TRACK_NUMERIC_FIELD_KEYS = [
  'lowerPrice',
  'upperPrice',
  'longExposureUnits',
  'shortExposureUnits',
  'notionalPerUnit',
  'maxNotional',
  'minRebalanceUnits',
  'leverage',
  'dailyLossLimit',
  'totalLossLimit',
] as const;

type TrackNumericFieldKey = (typeof TRACK_NUMERIC_FIELD_KEYS)[number];

export function createTrackDraft(input: CreateTrackDraftInput): TrackDraft {
  const rawNumbers = extractRawNumbers(input.raw);

  return {
    draftId: input.draftId,
    additional: input.additional ?? {
      trackId: input.raw.trackId,
      symbol: input.raw.symbol,
    },
    rawNumbers,
    parsedNumbers: input.parsedNumbers ?? {},
    enums: input.enums ?? {
      shapeFamily: input.raw.shapeFamily,
      outOfBandPolicy: input.raw.outOfBandPolicy,
    },
    ui: {
      quotePriceInput: input.ui?.quotePriceInput ?? '',
    },
    attachments: input.attachments ?? {},
  };
}

export function buildTrackDraftSnapshot(draft: TrackDraft): TrackDraftParsedSnapshot {
  return ensureTrackDraftParsed(draft);
}

function extractRawNumbers(
  raw: CreateTrackDraftInput['raw'],
): TrackDraftRawNumericFields {
  const entries = TRACK_NUMERIC_FIELD_KEYS.map((key) => [key, raw[key]]);
  return Object.fromEntries(entries) as Record<TrackNumericFieldKey, string>;
}
