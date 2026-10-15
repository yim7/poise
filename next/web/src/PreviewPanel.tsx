// 预览面板（设计第 2.4 节第 2 步）：校验结果、批次表、格子表、启动后立即成交的订单。

import { decimal } from './format'
import { BatchTable, CellTable, ImmediateFillTable } from './tables'
import type { DraftPreview } from './types'

export function PreviewPanel({ preview }: { preview: DraftPreview }) {
  return (
    <section className="preview">
      <h2>预览</h2>
      <dl className="facts">
        <div>
          <dt>参数版本</dt>
          <dd>{preview.config_version}</dd>
        </div>
        <div>
          <dt>标记价</dt>
          <dd>{decimal(preview.market_price)}</dd>
        </div>
        <div>
          <dt>交易所仓位</dt>
          <dd>{decimal(preview.position)}</dd>
        </div>
        <div>
          <dt>可用 USDC</dt>
          <dd>{decimal(preview.available_collateral)}</dd>
        </div>
      </dl>
      {preview.validation.valid ? (
        <p className="ok">参数校验通过。</p>
      ) : (
        <p className="error">参数校验不通过：{preview.validation.reason}</p>
      )}
      {preview.validation.valid ? (
        <>
          <ImmediateFillTable fills={preview.immediate_fills} />
          <BatchTable batches={preview.batches} />
          <CellTable cells={preview.cells} />
        </>
      ) : null}
    </section>
  )
}
