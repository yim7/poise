// 预览（设计第 2.4 节第 2 步）：校验结果、关键读数、满仓压力测试、启动后立即成交和第一步
// 挂出的订单、批次和格子，以及交易所上已有的其他挂单（第 8 节）。
//
// 先放能不能启、风险多大，再放会发生什么，最后是划分的明细。

import { Callout } from '@/components/layout'
import { RangeBar } from '@/components/range-bar'
import { Stat, positionTone } from '@/components/stat'
import { decimal } from './format'
import {
  BatchTable,
  CellLadder,
  ForeignOrderTable,
  ImmediateFillTable,
  Panel,
  PlannedOrderTable,
  StressTable,
} from './tables'
import type { DraftPreview, StrategyConfig } from './types'

export function PreviewPanel({ preview, config }: { preview: DraftPreview; config: StrategyConfig | null }) {
  const valid = preview.validation.valid
  return (
    <section aria-label="预览" className="flex flex-col gap-4">
      <div className="flex items-baseline justify-between gap-3">
        <h2 className="font-heading text-lg font-semibold tracking-tight">预览</h2>
        <span className="num text-[11px] text-muted-foreground">参数版本 v{preview.config_version}</span>
      </div>

      {valid ? (
        <Callout tone="ok" title="校验通过" />
      ) : (
        <Callout tone="danger" title="校验不通过">
          {preview.validation.reason}
        </Callout>
      )}
      {preview.warnings.map((warning) => (
        <Callout key={warning} tone="warn">
          {warning}
        </Callout>
      ))}

      <div className="flex flex-col gap-4 rounded-lg bg-card p-4 ring-1 ring-border">
        <div className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-4">
          <Stat label="标记价" value={decimal(preview.market_price)} tone="signal" size="lg" />
          <Stat
            label="交易所仓位"
            value={decimal(preview.position)}
            tone={positionTone(preview.position)}
            size="lg"
          />
          <Stat label="可用 USDC" value={decimal(preview.available_collateral)} size="lg" />
          <Stat
            label="启动后立即成交"
            value={valid ? `${preview.immediate_fills.length} 张` : '—'}
            tone={preview.immediate_fills.length > 0 ? 'warn' : 'default'}
            size="lg"
          />
        </div>
        {config === null ? null : <RangeBar config={config} mark={preview.market_price} />}
      </div>

      {preview.stress.length === 0 ? null : (
        <Panel title="满仓压力测试">
          <StressTable sides={preview.stress} />
        </Panel>
      )}

      {valid ? (
        <>
          {Number(preview.position) !== 0 ? (
            <Callout tone="info">已有仓位，启动时按接管规则分给批次和格子</Callout>
          ) : null}
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            <Panel title="启动后立即成交" count={preview.immediate_fills.length}>
              <ImmediateFillTable fills={preview.immediate_fills} />
            </Panel>
            <Panel title="第一步挂出的订单" count={preview.planned_orders.length}>
              <PlannedOrderTable orders={preview.planned_orders} />
            </Panel>
          </div>
          {/* 预览的格子表多一列利润，放不进半宽，格子和批次各占一行。 */}
          <Panel title="网格格子" count={preview.cells.length}>
            <CellLadder cells={preview.cells} mark={preview.market_price} profit />
          </Panel>
          <Panel title="主仓批次" count={preview.batches.length}>
            <BatchTable batches={preview.batches} />
          </Panel>
        </>
      ) : null}

      {preview.foreign_orders.length === 0 ? null : (
        <Panel
          title="交易所上的其他挂单"
          count={preview.foreign_orders.length}
          aside={<span className="text-[11px] text-warn">本系统不撤销</span>}
        >
          <ForeignOrderTable orders={preview.foreign_orders} />
        </Panel>
      )}
    </section>
  )
}
