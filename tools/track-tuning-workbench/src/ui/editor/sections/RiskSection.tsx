import { InlineNotice } from '@/ui/common/InlineNotice';
import { commitOnEnter, type RiskSectionProps } from '@/ui/editor/TrackEditor';

export function RiskSection({
  bandProtectionKind,
  dailyLossLimit,
  totalLossLimit,
  dailyLossIssues,
  totalLossIssues,
  onBandProtectionKindChange,
  onDailyLossLimitChange,
  onTotalLossLimitChange,
  onCommit,
}: RiskSectionProps) {

  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">止损与带外策略</p>
      </div>

      <div className="field-grid field-grid--two field-grid--editor">
        <label className="field">
          <span className="field__label">带外策略</span>
          <select
            className="field__input"
            value={bandProtectionKind}
            onChange={(event) => {
              onBandProtectionKindChange(
                event.target.value as RiskSectionProps['bandProtectionKind'],
              );
              onCommit();
            }}
          >
            <option value="freeze">freeze</option>
            <option value="flatten">flatten</option>
            <option value="terminate">terminate</option>
          </select>
        </label>
        <label className="field">
          <span className="field__label">日内止损</span>
          <input
            className={dailyLossIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={dailyLossLimit}
            onChange={(event) => onDailyLossLimitChange(event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
        <label className="field">
          <span className="field__label">累计止损</span>
          <input
            className={totalLossIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={totalLossLimit}
            onChange={(event) => onTotalLossLimitChange(event.target.value)}
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
