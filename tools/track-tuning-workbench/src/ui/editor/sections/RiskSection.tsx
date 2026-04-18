import { InlineNotice } from '@/ui/common/InlineNotice';
import { commitOnEnter, fieldIssues, type TrackEditorSectionProps } from '@/ui/editor/TrackEditor';

export function RiskSection({
  draft,
  issuesByField,
  onEnumChange,
  onNumericChange,
  onCommit,
}: TrackEditorSectionProps) {
  const dailyLossIssues = fieldIssues(issuesByField, 'dailyLossLimit');
  const totalLossIssues = fieldIssues(issuesByField, 'totalLossLimit');

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">止损与带外策略</p>
        <h3 className="editor-section__title">风险预算</h3>
      </div>

      <div className="field-grid field-grid--three">
        <label className="field">
          <span className="field__label">带外策略</span>
          <select
            className="field__input"
            value={draft.enums.outOfBandPolicy}
            onChange={(event) => {
              onEnumChange('outOfBandPolicy', event.target.value);
              onCommit();
            }}
          >
            <option value="freeze">freeze</option>
            <option value="hold">hold</option>
            <option value="flatten">flatten</option>
            <option value="terminate">terminate</option>
          </select>
        </label>
        <label className="field">
          <span className="field__label">日内止损</span>
          <input
            className={dailyLossIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={draft.rawNumbers.dailyLossLimit}
            onChange={(event) => onNumericChange('dailyLossLimit', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
        <label className="field">
          <span className="field__label">累计止损</span>
          <input
            className={totalLossIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={draft.rawNumbers.totalLossLimit}
            onChange={(event) => onNumericChange('totalLossLimit', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
      </div>

      {dailyLossIssues.length > 0 || totalLossIssues.length > 0 ? (
        <InlineNotice tone="danger" title="风险预算需要修正">
          <ul className="inline-notice__list">
            {[...dailyLossIssues, ...totalLossIssues].map((message) => (
              <li key={message}>{message}</li>
            ))}
          </ul>
        </InlineNotice>
      ) : null}
    </section>
  );
}
