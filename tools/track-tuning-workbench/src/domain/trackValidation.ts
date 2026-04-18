import type {
  TrackDraft,
  TrackDraftNumericFields,
  TrackDraftParsedSnapshot,
} from '@/domain/trackDraft';
import { TRACK_NUMERIC_FIELD_KEYS } from '@/domain/trackDraft';

const FIELD_LABELS: Record<keyof TrackDraftNumericFields | 'quotePriceInput', string> = {
  lowerPrice: 'lower_price',
  upperPrice: 'upper_price',
  longExposureUnits: 'long_exposure_units',
  shortExposureUnits: 'short_exposure_units',
  notionalPerUnit: 'notional_per_unit',
  maxNotional: 'max_notional',
  minRebalanceUnits: 'min_rebalance_units',
  leverage: 'leverage',
  dailyLossLimit: 'daily_loss_limit',
  totalLossLimit: 'total_loss_limit',
  quotePriceInput: 'quote_price',
};

export interface TrackDraftIssue {
  field: keyof TrackDraftNumericFields | 'quotePriceInput';
  message: string;
}

export interface TrackDraftValidationResult {
  isValid: boolean;
  issues: TrackDraftIssue[];
  parsed?: TrackDraftParsedSnapshot;
}

export function validateTrackDraft(draft: TrackDraft): TrackDraftValidationResult {
  const issues: TrackDraftIssue[] = [];
  const parsedNumbers = {} as TrackDraftNumericFields;

  for (const field of TRACK_NUMERIC_FIELD_KEYS) {
    const value = resolveNumericField(draft, field);
    if (value === null) {
      issues.push({
        field,
        message: `${FIELD_LABELS[field]} 必须是有限数字`,
      });
      continue;
    }
    parsedNumbers[field] = value;
  }

  const quotePrice = parseFiniteNumber(draft.ui.quotePriceInput);
  if (quotePrice === null) {
    issues.push({
      field: 'quotePriceInput',
      message: `${FIELD_LABELS.quotePriceInput} 必须是有限数字`,
    });
  }

  if (issues.length === 0) {
    validateStrategySemantics(parsedNumbers, issues);
  }

  if (issues.length > 0 || quotePrice === null) {
    return {
      isValid: false,
      issues,
    };
  }

  return {
    isValid: true,
    issues,
    parsed: {
      draftId: draft.draftId,
      additional: draft.additional,
      parsedNumbers,
      enums: draft.enums,
      ui: {
        quotePriceInput: draft.ui.quotePriceInput,
        quotePrice,
      },
      attachments: draft.attachments,
    },
  };
}

export function ensureTrackDraftParsed(draft: TrackDraft): TrackDraftParsedSnapshot {
  const result = validateTrackDraft(draft);
  if (!result.parsed) {
    const message = result.issues.map((issue) => issue.message).join('；');
    throw new Error(message || 'track draft 仍有未解决的输入错误');
  }
  return result.parsed;
}

export function parseFiniteNumber(input: string): number | null {
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

function resolveNumericField<K extends keyof TrackDraftNumericFields>(
  draft: TrackDraft,
  field: K,
): number | null {
  const parsedValue = draft.parsedNumbers[field];
  if (typeof parsedValue === 'number' && Number.isFinite(parsedValue)) {
    return parsedValue;
  }

  return parseFiniteNumber(draft.rawNumbers[field]);
}

function validateStrategySemantics(
  numbers: TrackDraftNumericFields,
  issues: TrackDraftIssue[],
) {
  if (numbers.lowerPrice >= numbers.upperPrice) {
    issues.push({
      field: 'lowerPrice',
      message: 'lower_price 必须小于 upper_price',
    });
  }

  if (numbers.longExposureUnits < 0 || numbers.shortExposureUnits < 0) {
    issues.push({
      field: numbers.longExposureUnits < 0 ? 'longExposureUnits' : 'shortExposureUnits',
      message: '容量不能为负数',
    });
  }

  if (numbers.notionalPerUnit <= 0) {
    issues.push({
      field: 'notionalPerUnit',
      message: 'notional_per_unit 必须大于 0',
    });
  }

  if (numbers.minRebalanceUnits < 0) {
    issues.push({
      field: 'minRebalanceUnits',
      message: 'min_rebalance_units 不能为负数',
    });
  }
}
