import { commitOnEnter, fieldIssues, type TrackEditorSectionProps } from '@/ui/editor/TrackEditor';

const FIELDS = [
  { key: 'longExposureUnits', label: '多头容量' },
  { key: 'shortExposureUnits', label: '空头容量' },
  { key: 'notionalPerUnit', label: '每单位名义仓位' },
  { key: 'maxNotional', label: '最大名义仓位' },
  { key: 'minRebalanceUnits', label: '最小调仓单位' },
  { key: 'leverage', label: '杠杆' },
] as const;

export function ExposureSection({
  draft,
  issuesByField,
  onNumericChange,
  onCommit,
}: TrackEditorSectionProps) {
  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">仓位与调仓</p>
        <h3 className="editor-section__title">容量、名义与最小步长</h3>
      </div>

      <div className="field-grid field-grid--three">
        {FIELDS.map((field) => {
          const issues = fieldIssues(issuesByField, field.key);
          return (
            <label className="field" key={field.key}>
              <span className="field__label">{field.label}</span>
              <input
                className={issues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
                value={draft.rawNumbers[field.key]}
                onChange={(event) => onNumericChange(field.key, event.target.value)}
                onBlur={onCommit}
                onKeyDown={(event) => commitOnEnter(event, onCommit)}
              />
            </label>
          );
        })}
      </div>
    </section>
  );
}
