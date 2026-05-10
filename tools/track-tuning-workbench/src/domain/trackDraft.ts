export type TrackShapeFamily = 'linear' | 'inertial' | 'responsive';
export type TrackBandProtectionKind = 'freeze' | 'flatten' | 'terminate';
export type BandFlattenTriggerPayload = 'immediate' | { flatten_confirm: { bps: number } };
export type BandRecoverPolicyPayload = 'back_in_band' | { reentry_confirm: { bps: number } };
export type BandProtectionPolicyPayload =
  | 'freeze'
  | { flatten: { trigger: BandFlattenTriggerPayload; recover: BandRecoverPolicyPayload } }
  | 'terminate';

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

export interface RiskAcquisitionDraft {
  initialRatio: string;
  advantageSteps: string;
  minReleaseSteps: string;
  maxReleaseSteps: string;
  catchupRatio: string;
}

export interface RiskAcquisitionParsed {
  initialRatio: number;
  advantageSteps: number;
  minReleaseSteps: number;
  maxReleaseSteps: number;
  catchupRatio: number;
}

export type RiskAcquisitionDraftField = keyof RiskAcquisitionDraft;
export type RiskAcquisitionFieldKey =
  | 'riskAcquisition.initialRatio'
  | 'riskAcquisition.advantageSteps'
  | 'riskAcquisition.minReleaseSteps'
  | 'riskAcquisition.maxReleaseSteps'
  | 'riskAcquisition.catchupRatio';

export interface TrackDraftEnumFields {
  shapeFamily: TrackShapeFamily;
  bandProtectionPolicy: BandProtectionPolicyPayload;
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
  | 'quotePriceInput'
  | RiskAcquisitionFieldKey;

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
  exchangeVenue?: string;
  exchangeRules?: TrackExchangeRulesDraft;
  lossGuard?: TrackLossGuardDraft;
  loadIssues?: TrackDraftLoadIssue[];
}

export const DEFAULT_BINANCE_FUTURES_EXCHANGE_RULES = Object.freeze({
  makerFeeRate: 0.0002,
  takerFeeRate: 0.0005,
});

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
  riskAcquisition: RiskAcquisitionDraft;
  parsedNumbers: Partial<TrackDraftNumericFields>;
  enums: TrackDraftEnumFields;
  ui: TrackDraftUiState;
  attachments: TrackDraftAttachments;
}

export interface TrackDraftParsedSnapshot {
  draftId: string;
  additional: TrackDraftAdditionalFields;
  parsedNumbers: TrackDraftNumericFields;
  riskAcquisition: RiskAcquisitionParsed;
  enums: TrackDraftEnumFields;
  ui: TrackDraftResolvedUiState;
  attachments: TrackDraftAttachments;
}

export interface CreateTrackDraftInput {
  draftId: string;
  raw: TrackDraftAdditionalFields &
    TrackDraftRawNumericFields & {
      shapeFamily: TrackShapeFamily;
      bandProtectionPolicy: BandProtectionPolicyPayload;
    };
  parsedNumbers?: Partial<TrackDraftNumericFields>;
  riskAcquisition?: RiskAcquisitionDraft;
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

export const RISK_ACQUISITION_FIELD_KEYS = [
  'initialRatio',
  'advantageSteps',
  'minReleaseSteps',
  'maxReleaseSteps',
  'catchupRatio',
] as const satisfies readonly RiskAcquisitionDraftField[];

const RISK_ACQUISITION_FIELD_KEY_BY_DRAFT_FIELD: Record<
  RiskAcquisitionDraftField,
  RiskAcquisitionFieldKey
> = {
  initialRatio: 'riskAcquisition.initialRatio',
  advantageSteps: 'riskAcquisition.advantageSteps',
  minReleaseSteps: 'riskAcquisition.minReleaseSteps',
  maxReleaseSteps: 'riskAcquisition.maxReleaseSteps',
  catchupRatio: 'riskAcquisition.catchupRatio',
};

export const DEFAULT_RISK_ACQUISITION_DRAFT: RiskAcquisitionDraft = Object.freeze({
  initialRatio: '0.3',
  advantageSteps: '2',
  minReleaseSteps: '1',
  maxReleaseSteps: '4',
  catchupRatio: '0.25',
});

export function createTrackDraft(input: CreateTrackDraftInput): TrackDraft {
  const rawNumbers = extractRawNumbers(input.raw);
  const bandProtectionPolicy = input.enums?.bandProtectionPolicy ?? input.raw.bandProtectionPolicy;

  return {
    draftId: input.draftId,
    additional: input.additional ?? {
      trackId: input.raw.trackId,
      symbol: input.raw.symbol,
    },
    rawNumbers,
    riskAcquisition: {
      ...DEFAULT_RISK_ACQUISITION_DRAFT,
      ...(input.riskAcquisition ?? {}),
    },
    parsedNumbers: input.parsedNumbers ?? parseTrackDraftRawNumbers(rawNumbers),
    enums: {
      shapeFamily: input.enums?.shapeFamily ?? input.raw.shapeFamily,
      bandProtectionPolicy,
    },
    ui: {
      quotePriceInput: input.ui?.quotePriceInput ?? '',
    },
    attachments: withDefaultBinanceFuturesExchangeRules(input.attachments ?? {}),
  };
}

function extractRawNumbers(
  raw: CreateTrackDraftInput['raw'],
): TrackDraftRawNumericFields {
  const entries = TRACK_NUMERIC_FIELD_KEYS.map((key) => [key, raw[key]]);
  return Object.fromEntries(entries) as Record<TrackNumericFieldKey, string>;
}

export function bandProtectionKindFromPolicy(
  policy: BandProtectionPolicyPayload,
): TrackBandProtectionKind {
  if (typeof policy === 'string') {
    return policy;
  }
  return 'flatten';
}

export function defaultBandProtectionPolicy(
  kind: TrackBandProtectionKind,
): BandProtectionPolicyPayload {
  switch (kind) {
    case 'freeze':
      return 'freeze';
    case 'flatten':
      return {
        flatten: {
          trigger: {
            flatten_confirm: { bps: 500 },
          },
          recover: {
            reentry_confirm: { bps: 500 },
          },
        },
      };
    case 'terminate':
      return 'terminate';
  }
}

export function riskAcquisitionFieldKey(
  field: RiskAcquisitionDraftField,
): RiskAcquisitionFieldKey {
  return RISK_ACQUISITION_FIELD_KEY_BY_DRAFT_FIELD[field];
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

export function withDefaultBinanceFuturesExchangeRules(
  attachments: TrackDraftAttachments,
): TrackDraftAttachments {
  return {
    ...attachments,
    exchangeRules: {
      ...DEFAULT_BINANCE_FUTURES_EXCHANGE_RULES,
      ...attachments.exchangeRules,
    },
  };
}
