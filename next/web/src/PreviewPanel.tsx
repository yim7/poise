// 预览面板（设计第 2.4 节第 2 步）：校验结果、批次表、格子表、启动后立即成交的订单、
// 第一步挂出的订单，以及交易所上已有的其他挂单（第 8 节）。

import { decimal } from './format'
import {
  BatchTable,
  CellTable,
  ForeignOrderTable,
  ImmediateFillTable,
  PlannedOrderTable,
} from './tables'
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
          {Number(preview.position) !== 0 ? (
            <p>标的上已有仓位，启动时按靠近空隙的一端分给批次和格子，见下面两张表。</p>
          ) : null}
          <ImmediateFillTable fills={preview.immediate_fills} />
          <PlannedOrderTable orders={preview.planned_orders} />
          <BatchTable batches={preview.batches} />
          <CellTable cells={preview.cells} />
        </>
      ) : null}
      <ForeignOrderTable orders={preview.foreign_orders} />
    </section>
  )
}
