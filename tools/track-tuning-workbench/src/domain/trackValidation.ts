import type {
  TrackDraft,
  TrackDraftFieldKey,
  TrackDraftNumericFields,
  TrackDraftParsedSnapshot,
  RiskAcquisitionDraftField,
  RiskAcquisitionParsed,
} from '@/domain/trackDraft';
import {
  RISK_ACQUISITION_FIELD_KEYS,
  TRACK_NUMERIC_FIELD_KEYS,
  riskAcquisitionFieldKey,
} from '@/domain/trackDraft';

const FIELD_LABELS: Record<
  TrackDraftFieldKey,
  string
> = {
  trackId: 'track_id',
  symbol: 'symbol',
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
  shapeFamily: 'shape_family',
  bandProtectionKind: 'out_of_band_policy',
  quotePriceInput: 'quote_price',
  'riskAcquisition.initialRatio': 'risk_acquisition.initial_ratio',
  'riskAcquisition.advantageSteps':
    'risk_acquisition.advantage_steps',
  'riskAcquisition.minReleaseSteps':
    'risk_acquisition.min_release_steps',
  'riskAcquisition.maxReleaseSteps':
    'risk_acquisition.max_release_steps',
  'riskAcquisition.catchupRatio': 'risk_acquisition.catchup_ratio',
};

export interface TrackDraftIssue {
  field: TrackDraftFieldKey;
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
  let riskAcquisition: RiskAcquisitionParsed | undefined;

  for (const issue of draft.attachments.loadIssues ?? []) {
    issues.push({
      field: issue.field,
      message: issue.message,
    });
  }

  if (draft.additional.trackId.trim().length === 0) {
    issues.push({
      field: 'trackId',
      message: `${FIELD_LABELS.trackId} 不能为空`,
    });
  }

  if (draft.additional.symbol.trim().length === 0) {
    issues.push({
      field: 'symbol',
      message: `${FIELD_LABELS.symbol} 不能为空`,
    });
  }

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

  riskAcquisition = resolveRiskAcquisition(draft, issues);

  const hasQuoteInput = draft.ui.quotePriceInput.trim().length > 0;
  const quotePrice = parseFiniteNumber(draft.ui.quotePriceInput);
  if (hasQuoteInput && quotePrice === null) {
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
      riskAcquisition: riskAcquisition!,
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

export function buildTrackDraftSnapshot(draft: TrackDraft): TrackDraftParsedSnapshot {
  return ensureTrackDraftParsed(draft);
}

export function clearResolvedLoadIssues(draft: TrackDraft, editedField: TrackDraftFieldKey) {
  const activeIssues = (draft.attachments.loadIssues ?? []).filter((issue) => issue.field !== editedField);
  if (activeIssues.length === 0) {
    delete draft.attachments.loadIssues;
    return;
  }
  draft.attachments.loadIssues = activeIssues;
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

  if (numbers.longExposureUnits + numbers.shortExposureUnits <= Number.EPSILON) {
    issues.push({
      field: 'longExposureUnits',
      message: 'long_exposure_units 和 short_exposure_units 不能同时为 0',
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

  if (numbers.maxNotional <= 0) {
    issues.push({
      field: 'maxNotional',
      message: 'max_notional 必须大于 0',
    });
  }

  if (numbers.dailyLossLimit <= 0) {
    issues.push({
      field: 'dailyLossLimit',
      message: 'daily_loss_limit 必须大于 0',
    });
  }

  if (numbers.totalLossLimit <= 0) {
    issues.push({
      field: 'totalLossLimit',
      message: 'total_loss_limit 必须大于 0',
    });
  }
}

function resolveRiskAcquisition(
  draft: TrackDraft,
  issues: TrackDraftIssue[],
): RiskAcquisitionParsed | undefined {
  const parsed = {} as RiskAcquisitionParsed;

  for (const field of RISK_ACQUISITION_FIELD_KEYS) {
    const issueField = riskAcquisitionFieldKey(field);
    const value = parseFiniteNumber(draft.riskAcquisition[field]);
    if (value === null) {
      issues.push({
        field: issueField,
        message: `${FIELD_LABELS[issueField]} 必须是有限数字`,
      });
      continue;
    }
    parsed[field] = value;
  }

  if (hasRiskAcquisitionParseIssue(issues)) {
    return undefined;
  }

  validateRiskAcquisitionSemantics(parsed, issues);
  return parsed;
}

function hasRiskAcquisitionParseIssue(issues: TrackDraftIssue[]): boolean {
  return issues.some((issue) =>
    issue.field.startsWith('riskAcquisition.'),
  );
}

function validateRiskAcquisitionSemantics(
  delay: RiskAcquisitionParsed,
  issues: TrackDraftIssue[],
) {
  requireRatioInUnitInterval(
    delay.initialRatio,
    'initialRatio',
    issues,
  );
  requirePositive(
    delay.advantageSteps,
    'advantageSteps',
    issues,
  );
  requirePositive(
    delay.minReleaseSteps,
    'minReleaseSteps',
    issues,
  );
  if (delay.maxReleaseSteps < delay.minReleaseSteps) {
    const field = riskAcquisitionFieldKey('maxReleaseSteps');
    issues.push({
      field,
      message: `${FIELD_LABELS[field]} 必须大于等于 ${FIELD_LABELS[riskAcquisitionFieldKey('minReleaseSteps')]}`,
    });
  }
  requireRatioInUnitInterval(delay.catchupRatio, 'catchupRatio', issues);
}

function requireRatioInUnitInterval(
  value: number,
  field: RiskAcquisitionDraftField,
  issues: TrackDraftIssue[],
) {
  if (value <= 0 || value > 1) {
    const issueField = riskAcquisitionFieldKey(field);
    issues.push({
      field: issueField,
      message: `${FIELD_LABELS[issueField]} 必须在 (0, 1] 内`,
    });
  }
}

function requirePositive(
  value: number,
  field: RiskAcquisitionDraftField,
  issues: TrackDraftIssue[],
) {
  if (value <= 0) {
    const issueField = riskAcquisitionFieldKey(field);
    issues.push({
      field: issueField,
      message: `${FIELD_LABELS[issueField]} 必须大于 0`,
    });
  }
}
