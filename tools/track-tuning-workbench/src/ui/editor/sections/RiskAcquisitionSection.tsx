import { InlineNotice } from '@/ui/common/InlineNotice';
import {
  commitOnEnter,
  type RiskAcquisitionSectionProps,
} from '@/ui/editor/TrackEditor';

const FIELDS = [
  { key: 'initialRatio', label: '启动初始比例' },
  { key: 'advantageSteps', label: '优势倍数' },
  { key: 'minReleaseSteps', label: '最小释放倍数' },
  { key: 'maxReleaseSteps', label: '最大释放倍数' },
  { key: 'catchupRatio', label: '追补比例' },
  { key: 'staleReleaseMinutes', label: '时间释放分钟' },
] as const;

export function RiskAcquisitionSection({
  values,
  issuesByField,
  onFieldChange,
  onCommit,
}: RiskAcquisitionSectionProps) {
  const allIssues = Object.values(issuesByField).flat();

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">风险暴露获取</p>
      </div>

      <div className="field-grid field-grid--two field-grid--editor">
        {FIELDS.map((field) => {
          const issues = issuesByField[field.key];
          return (
            <label className="field" key={field.key}>
              <span className="field__label">{field.label}</span>
              <input
                className={issues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
                value={values[field.key]}
                onChange={(event) => onFieldChange(field.key, event.target.value)}
                onBlur={onCommit}
                onKeyDown={(event) => commitOnEnter(event, onCommit)}
              />
            </label>
          );
        })}
      </div>

      {allIssues.length > 0 ? (
        <InlineNotice tone="warning" title="获取参数需要修正">
          <ul className="inline-notice__list">
            {allIssues.map((message) => (
              <li key={message}>{message}</li>
            ))}
          </ul>
        </InlineNotice>
      ) : null}
    </section>
  );
}
