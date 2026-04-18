import type { KeyboardEvent } from 'react';

import type { TrackDraft, TrackDraftRawNumericFields } from '@/domain/trackDraft';
import type { TrackDraftIssue } from '@/domain/trackValidation';
import { InlineNotice } from '@/ui/common/InlineNotice';
import { CurveSection } from '@/ui/editor/sections/CurveSection';
import { ExposureSection } from '@/ui/editor/sections/ExposureSection';
import { IdentitySection } from '@/ui/editor/sections/IdentitySection';
import { PriceBandSection } from '@/ui/editor/sections/PriceBandSection';
import { RiskSection } from '@/ui/editor/sections/RiskSection';

export interface TrackEditorProps {
  draft: TrackDraft | null;
  issues: TrackDraftIssue[];
  onAdditionalChange(field: 'trackId' | 'symbol', value: string): void;
  onNumericChange(field: keyof TrackDraftRawNumericFields, value: string): void;
  onEnumChange(field: 'shapeFamily' | 'outOfBandPolicy', value: string): void;
  onQuotePriceChange(value: string): void;
  onCommit(): void;
}

export interface TrackEditorSectionProps {
  draft: TrackDraft;
  issuesByField: Map<string, string[]>;
  onAdditionalChange(field: 'trackId' | 'symbol', value: string): void;
  onNumericChange(field: keyof TrackDraftRawNumericFields, value: string): void;
  onEnumChange(field: 'shapeFamily' | 'outOfBandPolicy', value: string): void;
  onQuotePriceChange(value: string): void;
  onCommit(): void;
}

export function TrackEditor({
  draft,
  issues,
  onAdditionalChange,
  onNumericChange,
  onEnumChange,
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
          draft={draft}
          issuesByField={issuesByField}
          onAdditionalChange={onAdditionalChange}
          onNumericChange={onNumericChange}
          onEnumChange={onEnumChange}
          onQuotePriceChange={onQuotePriceChange}
          onCommit={onCommit}
        />
        <PriceBandSection
          draft={draft}
          issuesByField={issuesByField}
          onAdditionalChange={onAdditionalChange}
          onNumericChange={onNumericChange}
          onEnumChange={onEnumChange}
          onQuotePriceChange={onQuotePriceChange}
          onCommit={onCommit}
        />
        <ExposureSection
          draft={draft}
          issuesByField={issuesByField}
          onAdditionalChange={onAdditionalChange}
          onNumericChange={onNumericChange}
          onEnumChange={onEnumChange}
          onQuotePriceChange={onQuotePriceChange}
          onCommit={onCommit}
        />
        <RiskSection
          draft={draft}
          issuesByField={issuesByField}
          onAdditionalChange={onAdditionalChange}
          onNumericChange={onNumericChange}
          onEnumChange={onEnumChange}
          onQuotePriceChange={onQuotePriceChange}
          onCommit={onCommit}
        />
        <CurveSection
          draft={draft}
          issuesByField={issuesByField}
          onAdditionalChange={onAdditionalChange}
          onNumericChange={onNumericChange}
          onEnumChange={onEnumChange}
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
