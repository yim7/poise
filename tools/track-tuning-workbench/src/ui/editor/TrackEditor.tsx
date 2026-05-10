import type { KeyboardEvent } from 'react';

import {
  bandProtectionKindFromPolicy,
  riskIncreaseDelayFieldKey,
  type RiskIncreaseDelayDraft,
  type RiskIncreaseDelayDraftField,
  type TrackBandProtectionKind,
  type TrackDraft,
  type TrackDraftRawNumericFields,
} from '@/domain/trackDraft';
import type { TrackDraftIssue } from '@/domain/trackValidation';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { CurveSection } from '@/ui/editor/sections/CurveSection';
import { ExposureSection } from '@/ui/editor/sections/ExposureSection';
import { IdentitySection } from '@/ui/editor/sections/IdentitySection';
import { PriceBandSection } from '@/ui/editor/sections/PriceBandSection';
import { RiskIncreaseDelaySection } from '@/ui/editor/sections/RiskIncreaseDelaySection';
import { RiskSection } from '@/ui/editor/sections/RiskSection';

export interface TrackEditorProps {
  draft: TrackDraft | null;
  issues: TrackDraftIssue[];
  onAdditionalChange(field: 'trackId' | 'symbol', value: string): void;
  onNumericChange(field: keyof TrackDraftRawNumericFields, value: string): void;
  onEnumChange(field: 'shapeFamily' | 'bandProtectionKind', value: string): void;
  onRiskIncreaseDelayToggle(enabled: boolean): void;
  onRiskIncreaseDelayChange(field: RiskIncreaseDelayDraftField, value: string): void;
  onQuotePriceChange(value: string): void;
  onCommit(): void;
}

export interface IdentitySectionProps {
  trackId: string;
  symbol: string;
  trackIdIssues: string[];
  symbolIssues: string[];
  onTrackIdChange(value: string): void;
  onSymbolChange(value: string): void;
  onCommit(): void;
}

export interface PriceBandSectionProps {
  lowerPrice: string;
  upperPrice: string;
  lowerIssues: string[];
  upperIssues: string[];
  onLowerPriceChange(value: string): void;
  onUpperPriceChange(value: string): void;
  onCommit(): void;
}

export interface ExposureSectionProps {
  values: Pick<
    TrackDraft['rawNumbers'],
    | 'longExposureUnits'
    | 'shortExposureUnits'
    | 'notionalPerUnit'
    | 'maxNotional'
    | 'minRebalanceUnits'
    | 'leverage'
  >;
  issuesByField: Record<
    | 'longExposureUnits'
    | 'shortExposureUnits'
    | 'notionalPerUnit'
    | 'maxNotional'
    | 'minRebalanceUnits'
    | 'leverage',
    string[]
  >;
  onNumericChange(field: keyof ExposureSectionProps['values'], value: string): void;
  onCommit(): void;
}

export interface RiskSectionProps {
  bandProtectionKind: TrackBandProtectionKind;
  dailyLossLimit: string;
  totalLossLimit: string;
  dailyLossIssues: string[];
  totalLossIssues: string[];
  onBandProtectionKindChange(value: TrackBandProtectionKind): void;
  onDailyLossLimitChange(value: string): void;
  onTotalLossLimitChange(value: string): void;
  onCommit(): void;
}

export interface RiskIncreaseDelaySectionProps {
  enabled: boolean;
  values: RiskIncreaseDelayDraft | undefined;
  issuesByField: Record<RiskIncreaseDelayDraftField, string[]>;
  onEnabledChange(value: boolean): void;
  onDelayFieldChange(field: RiskIncreaseDelayDraftField, value: string): void;
  onCommit(): void;
}

export interface CurveSectionProps {
  shapeFamily: TrackDraft['enums']['shapeFamily'];
  quotePriceInput: string;
  exchangeVenue: string;
  quoteIssues: string[];
  onShapeFamilyChange(value: TrackDraft['enums']['shapeFamily']): void;
  onQuotePriceChange(value: string): void;
  onCommit(): void;
}

export function TrackEditor({
  draft,
  issues,
  onAdditionalChange,
  onNumericChange,
  onEnumChange,
  onRiskIncreaseDelayToggle,
  onRiskIncreaseDelayChange,
  onQuotePriceChange,
  onCommit,
}: TrackEditorProps) {
  if (!draft) {
    return (
      <section className="workbench-panel workbench-panel--editor" aria-label="参数编辑区">
        <div className="workbench-panel__header">
          <div>
            <p className="workbench-panel__eyebrow">参数编辑</p>
            <h2 className="workbench-panel__title">等待选中 Track</h2>
          </div>
        </div>
        <div className="empty-state empty-state--wide">
          <p className="empty-state__title">当前没有可编辑的 Track</p>
          <p className="empty-state__body">左栏选中一个 Track 后，这里会展开全部可调参数。</p>
        </div>
      </section>
    );
  }

  const issuesByField = new Map<string, string[]>();
  for (const issue of issues) {
    const messages = issuesByField.get(issue.field) ?? [];
    messages.push(issue.message);
    issuesByField.set(issue.field, messages);
  }

  return (
    <section className="workbench-panel workbench-panel--editor" aria-label="参数编辑区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">参数编辑</p>
          <h2 className="workbench-panel__title">围绕主图直接调参</h2>
        </div>
      </div>

      {issues.length > 0 ? (
        <InlineNotice tone="warning" title="输入仍可继续，但这些字段需要修正">
          当前共有 {issues.length} 处输入待修正。字段级提示会保留在对应分组里，方便继续编辑时逐项收敛。
        </InlineNotice>
      ) : null}

      <div className="editor-sections">
        <IdentitySection
          trackId={draft.additional.trackId}
          symbol={draft.additional.symbol}
          trackIdIssues={fieldIssues(issuesByField, 'trackId')}
          symbolIssues={fieldIssues(issuesByField, 'symbol')}
          onTrackIdChange={(value) => onAdditionalChange('trackId', value)}
          onSymbolChange={(value) => onAdditionalChange('symbol', value)}
          onCommit={onCommit}
        />
        <PriceBandSection
          lowerPrice={draft.rawNumbers.lowerPrice}
          upperPrice={draft.rawNumbers.upperPrice}
          lowerIssues={fieldIssues(issuesByField, 'lowerPrice')}
          upperIssues={fieldIssues(issuesByField, 'upperPrice')}
          onLowerPriceChange={(value) => onNumericChange('lowerPrice', value)}
          onUpperPriceChange={(value) => onNumericChange('upperPrice', value)}
          onCommit={onCommit}
        />
        <ExposureSection
          values={{
            longExposureUnits: draft.rawNumbers.longExposureUnits,
            shortExposureUnits: draft.rawNumbers.shortExposureUnits,
            notionalPerUnit: draft.rawNumbers.notionalPerUnit,
            maxNotional: draft.rawNumbers.maxNotional,
            minRebalanceUnits: draft.rawNumbers.minRebalanceUnits,
            leverage: draft.rawNumbers.leverage,
          }}
          issuesByField={{
            longExposureUnits: fieldIssues(issuesByField, 'longExposureUnits'),
            shortExposureUnits: fieldIssues(issuesByField, 'shortExposureUnits'),
            notionalPerUnit: fieldIssues(issuesByField, 'notionalPerUnit'),
            maxNotional: fieldIssues(issuesByField, 'maxNotional'),
            minRebalanceUnits: fieldIssues(issuesByField, 'minRebalanceUnits'),
            leverage: fieldIssues(issuesByField, 'leverage'),
          }}
          onNumericChange={onNumericChange}
          onCommit={onCommit}
        />
        <RiskSection
          bandProtectionKind={bandProtectionKindFromPolicy(draft.enums.bandProtectionPolicy)}
          dailyLossLimit={draft.rawNumbers.dailyLossLimit}
          totalLossLimit={draft.rawNumbers.totalLossLimit}
          dailyLossIssues={fieldIssues(issuesByField, 'dailyLossLimit')}
          totalLossIssues={fieldIssues(issuesByField, 'totalLossLimit')}
          onBandProtectionKindChange={(value) => onEnumChange('bandProtectionKind', value)}
          onDailyLossLimitChange={(value) => onNumericChange('dailyLossLimit', value)}
          onTotalLossLimitChange={(value) => onNumericChange('totalLossLimit', value)}
          onCommit={onCommit}
        />
        <RiskIncreaseDelaySection
          enabled={Boolean(draft.riskIncreaseDelay)}
          values={draft.riskIncreaseDelay}
          issuesByField={{
            startupInitialRatio: fieldIssues(
              issuesByField,
              riskIncreaseDelayFieldKey('startupInitialRatio'),
            ),
            advantageMinRebalanceMultiples: fieldIssues(
              issuesByField,
              riskIncreaseDelayFieldKey('advantageMinRebalanceMultiples'),
            ),
            baseStepMinRebalanceMultiples: fieldIssues(
              issuesByField,
              riskIncreaseDelayFieldKey('baseStepMinRebalanceMultiples'),
            ),
            maxStepMinRebalanceMultiples: fieldIssues(
              issuesByField,
              riskIncreaseDelayFieldKey('maxStepMinRebalanceMultiples'),
            ),
            catchupRatio: fieldIssues(
              issuesByField,
              riskIncreaseDelayFieldKey('catchupRatio'),
            ),
          }}
          onEnabledChange={onRiskIncreaseDelayToggle}
          onDelayFieldChange={onRiskIncreaseDelayChange}
          onCommit={onCommit}
        />
        <CurveSection
          shapeFamily={draft.enums.shapeFamily}
          quotePriceInput={draft.ui.quotePriceInput}
          exchangeVenue={draft.attachments.exchangeVenue?.trim() || 'binance'}
          quoteIssues={fieldIssues(issuesByField, 'quotePriceInput')}
          onShapeFamilyChange={(value) => onEnumChange('shapeFamily', value)}
          onQuotePriceChange={onQuotePriceChange}
          onCommit={onCommit}
        />
      </div>
    </section>
  );
}

export function commitOnEnter(
  event: KeyboardEvent<HTMLInputElement | HTMLSelectElement>,
  onCommit: () => void,
) {
  if (event.key !== 'Enter') {
    return;
  }
  onCommit();
}

export function fieldIssues(
  issuesByField: Map<string, string[]>,
  field: string,
) {
  return issuesByField.get(field) ?? [];
}
