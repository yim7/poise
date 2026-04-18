import { InlineNotice } from '@/ui/common/InlineNotice';
import { commitOnEnter, type CurveSectionProps } from '@/ui/editor/TrackEditor';

export function CurveSection({
  shapeFamily,
  quotePriceInput,
  quoteIssues,
  onShapeFamilyChange,
  onQuotePriceChange,
  onCommit,
}: CurveSectionProps) {
  return (
    <section className="editor-section">
      <div className="editor-section__header">
        <p className="editor-section__eyebrow">曲线与预览</p>
        <h3 className="editor-section__title">曲线家族与试算锚点</h3>
      </div>

      <div className="field-grid field-grid--two">
        <label className="field">
          <span className="field__label">曲线家族</span>
          <select
            className="field__input"
            value={shapeFamily}
            onChange={(event) => {
              onShapeFamilyChange(event.target.value as CurveSectionProps['shapeFamily']);
              onCommit();
            }}
          >
            <option value="linear">linear</option>
            <option value="inertial">inertial</option>
            <option value="responsive">responsive</option>
          </select>
        </label>
        <label className="field">
          <span className="field__label">临时覆盖价格</span>
          <input
            className={quoteIssues.length > 0 ? 'field__input field__input--invalid' : 'field__input'}
            value={quotePriceInput}
            onChange={(event) => onQuotePriceChange(event.target.value)}
            onBlur={onCommit}
            onKeyDown={(event) => commitOnEnter(event, onCommit)}
          />
        </label>
      </div>

      {quoteIssues.length > 0 ? (
        <InlineNotice tone="warning" title="价格锚点仍在编辑中">
          <ul className="inline-notice__list">
            {quoteIssues.map((message) => (
              <li key={message}>{message}</li>
            ))}
          </ul>
        </InlineNotice>
      ) : (
        <InlineNotice title="预览说明">
          默认使用 Binance 合约实时价格；这里填入数字后，会只在本地试算里临时覆盖当前价格，不会写进导出配置。
        </InlineNotice>
      )}
    </section>
  );
}
