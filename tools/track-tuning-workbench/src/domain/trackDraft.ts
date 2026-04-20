export type TrackShapeFamily = 'linear' | 'inertial' | 'responsive';
export type TrackBandProtectionKind = 'freeze' | 'hold' | 'flatten' | 'terminate';

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
  bandProtectionKind: TrackBandProtectionKind;
}

export interface TrackDraftAdditionalFields {
  trackId: string;
  symbol: string;
}

export type TrackDraftFieldKey =
  | keyof TrackDraftNumericFields
  | 'trackId'
  | 'symbol'
  | 'shapeFamily'
  | 'bandProtectionKind'
  | 'quotePriceInput';

export interface TrackDraftLoadIssue {
  field: TrackDraftFieldKey;
  message: string;
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
  loadIssues?: TrackDraftLoadIssue[];
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
    parsedNumbers: input.parsedNumbers ?? parseTrackDraftRawNumbers(rawNumbers),
    enums: input.enums ?? {
      shapeFamily: input.raw.shapeFamily,
      bandProtectionKind: input.raw.bandProtectionKind,
    },
    ui: {
      quotePriceInput: input.ui?.quotePriceInput ?? '',
    },
    attachments: input.attachments ?? {},
  };
}

function extractRawNumbers(
  raw: CreateTrackDraftInput['raw'],
): TrackDraftRawNumericFields {
  const entries = TRACK_NUMERIC_FIELD_KEYS.map((key) => [key, raw[key]]);
  return Object.fromEntries(entries) as Record<TrackNumericFieldKey, string>;
}

export function refreshTrackDraftParsedNumbers(draft: TrackDraft) {
  draft.parsedNumbers = {
    ...draft.parsedNumbers,
    ...parseTrackDraftRawNumbers(draft.rawNumbers),
  };
}

export function parseTrackDraftRawNumbers(
  rawNumbers: TrackDraftRawNumericFields,
): Partial<TrackDraftNumericFields> {
  const parsed: Partial<TrackDraftNumericFields> = {};

  for (const field of TRACK_NUMERIC_FIELD_KEYS) {
    const value = parseFiniteNumber(rawNumbers[field]);
    if (value === null) {
      continue;
    }
    parsed[field] = value;
  }

  return parsed;
}

function parseFiniteNumber(input: string): number | null {
  const trimmed = input.trim();
  if (trimmed.length === 0) {
    return null;
  }

  const value = Number(trimmed);
  if (!Number.isFinite(value)) {
    return null;
  }

  return value;
}
