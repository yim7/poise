import { DEFAULT_RISK_INCREASE_DELAY_DRAFT } from '@/domain/trackDraft';
import { InlineNotice } from '@/ui/common/InlineNotice';
import {
  commitOnEnter,
  type RiskIncreaseDelaySectionProps,
} from '@/ui/editor/TrackEditor';

const FIELDS = [
  { key: 'startupInitialRatio', label: '启动初始比例' },
  { key: 'advantageMinRebalanceMultiples', label: '优势倍数' },
  { key: 'baseStepMinRebalanceMultiples', label: '最小释放倍数' },
  { key: 'maxStepMinRebalanceMultiples', label: '最大释放倍数' },
  { key: 'catchupRatio', label: '追补比例' },
] as const;

export function RiskIncreaseDelaySection({
  enabled,
  values,
  issuesByField,
  onEnabledChange,
  onDelayFieldChange,
  onCommit,
}: RiskIncreaseDelaySectionProps) {
  const displayValues = values ?? DEFAULT_RISK_INCREASE_DELAY_DRAFT;
  const allIssues = Object.values(issuesByField).flat();

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">增加风险延迟</p>
      </div>

      <label className="field field--checkbox">
        <input
          type="checkbox"
          checked={enabled}
          onChange={(event) => {
            onEnabledChange(event.target.checked);
            onCommit();
          }}
        />
        <span className="field__label">启用增加风险延迟</span>
      </label>

      {enabled ? (
        <div className="field-grid field-grid--two field-grid--editor">
          {FIELDS.map((field) => {
            const issues = issuesByField[field.key];
            return (
              <label className="field" key={field.key}>
                <span className="field__label">{field.label}</span>
                <input
                  className={issues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
                  value={displayValues[field.key]}
                  onChange={(event) => onDelayFieldChange(field.key, event.target.value)}
                  onBlur={onCommit}
                  onKeyDown={(event) => commitOnEnter(event, onCommit)}
                />
              </label>
            );
          })}
        </div>
      ) : (
        <InlineNotice title="当前未启用">
          增加风险暴露会按曲线立即追随；降低风险暴露仍按现有优先级执行。
        </InlineNotice>
      )}

      {allIssues.length > 0 ? (
        <InlineNotice tone="warning" title="延迟参数需要修正">
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
