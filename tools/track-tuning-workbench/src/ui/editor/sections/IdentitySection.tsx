import {
  commitOnEnter,
  fieldIssues,
  type TrackEditorSectionProps,
} from '@/ui/editor/TrackEditor';

export function IdentitySection({
  draft,
  issuesByField,
  onAdditionalChange,
  onCommit,
}: TrackEditorSectionProps) {
  const trackIdIssues = fieldIssues(issuesByField, 'trackId');
  const symbolIssues = fieldIssues(issuesByField, 'symbol');

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">标识</p>
        <h3 className="editor-section__title">Track 基本信息</h3>
      </div>

      <div className="field-grid field-grid--two">
        <label className="field">
          <span className="field__label">Track ID</span>
          <input
            className={trackIdIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            name="trackId"
            value={draft.additional.trackId}
            onChange={(event) => onAdditionalChange('trackId', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
        <label className="field">
          <span className="field__label">交易对</span>
          <input
            className={symbolIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            name="symbol"
            value={draft.additional.symbol}
            onChange={(event) => onAdditionalChange('symbol', event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
      </div>
    </section>
  );
}
