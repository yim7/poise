import { commitOnEnter, fieldIssues, type TrackEditorSectionProps } from '@/ui/editor/TrackEditor';

export function PriceBandSection({
  draft,
  issuesByField,
  onNumericChange,
  onCommit,
}: TrackEditorSectionProps) {
  const lowerIssues = fieldIssues(issuesByField, 'lowerPrice');
  const upperIssues = fieldIssues(issuesByField, 'upperPrice');

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">价格带</p>
        <h3 className="editor-section__title">带宽与边界</h3>
      </div>

      <div className="field-grid field-grid--two">
        <label className="field">
          <span className="field__label">下边界价格</span>
          <input
            className={lowerIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={draft.rawNumbers.lowerPrice}
            onChange={(event) => onNumericChange('lowerPrice', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
        <label className="field">
          <span className="field__label">上边界价格</span>
          <input
            className={upperIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={draft.rawNumbers.upperPrice}
            onChange={(event) => onNumericChange('upperPrice', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
      </div>
    </section>
  );
}
