// 格子阶梯、批次表、挂单表、预览里的几张表和参数版本表。
//
// 数字全部原样来自服务端：这里只排版，不重算。只有画进度条时把两个数相除，那是位置，
// 不是交易数量。

import { Pill, type Tone } from '@/components/badges'
import { Private, usePrivacy } from '@/components/privacy'
import { cn } from '@/lib/utils'
import {
  CELL_STATUS_LABELS,
  DIRECTION_LABELS,
  ORDER_STATUS_LABELS,
  PURPOSE_LABELS,
  SIDE_LABELS,
  orderSide,
  unitDirection,
  unitLabel,
} from './labels'
import { configSummary, decimal, percent, timestamp } from './format'
import type {
  BatchSummary,
  CellSummary,
  Direction,
  ExchangeOrder,
  ImmediateFill,
  Liquidation,
  OrderIntent,
  OrderStatus,
  Side,
  SideStress,
  StoredConfigVersion,
  StoredOrder,
  StrategyConfig,
  ThrottleInterval,
  Unit,
} from './types'

/** 一块带标题的面板：标题、数量、右上角的补充。 */
export function Panel({
  title,
  count,
  aside,
  children,
  className,
  bodyClassName,
}: {
  title: string
  count?: number
  aside?: React.ReactNode
  children: React.ReactNode
  className?: string
  bodyClassName?: string
}) {
  return (
    <section aria-label={title} className={cn('flex flex-col rounded-lg bg-card ring-1 ring-border', className)}>
      <header className="flex items-center justify-between gap-3 border-b border-border px-4 py-2.5">
        <h3 className="flex items-baseline gap-2 font-heading text-[13px] font-semibold tracking-tight">
          {title}
          {count === undefined ? null : (
            <span className="num text-[11px] font-normal text-muted-foreground">{count}</span>
          )}
        </h3>
        {aside}
      </header>
      <div className={cn('min-w-0', bodyClassName)}>{children}</div>
    </section>
  )
}

/** 表格的统一样式：紧凑、数字右对齐、表头小字。 */
function Table({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <div className="overflow-x-auto">
      <table
        className={cn(
          'w-full border-collapse text-xs',
          '[&_th]:h-8 [&_th]:px-2.5 [&_th]:text-right [&_th]:text-[11px] [&_th]:font-medium [&_th]:whitespace-nowrap [&_th]:text-muted-foreground',
          '[&_td]:h-8 [&_td]:px-2.5 [&_td]:text-right [&_td]:whitespace-nowrap',
          '[&_th:first-child]:pl-4 [&_td:first-child]:pl-4 [&_th:last-child]:pr-4 [&_td:last-child]:pr-4',
          '[&_th:first-child]:text-left [&_td:first-child]:text-left',
          '[&_tbody_tr]:border-t [&_tbody_tr]:border-border/70 [&_tbody_tr:hover]:bg-muted/40',
          className,
        )}
      >
        {children}
      </table>
    </div>
  )
}

function Empty({ children }: { children: React.ReactNode }) {
  return <p className="px-4 py-6 text-xs text-muted-foreground">{children}</p>
}

/** 方向的小标签：颜色和价格条上一致。 */
function DirectionTag({ direction }: { direction: Direction }) {
  return (
    <span
      className={cn(
        'inline-flex items-center gap-1.5 text-[11px]',
        direction === 'long' ? 'text-long' : 'text-short',
      )}
    >
      <span className={cn('h-3 w-0.5 rounded-full', direction === 'long' ? 'bg-long' : 'bg-short')} />
      {DIRECTION_LABELS[direction]}
    </span>
  )
}

/** 归属的简写（多3、空批0、多侧），带方向色条。 */
function UnitTag({ unit }: { unit: Unit }) {
  const direction = unitDirection(unit)
  return (
    <span
      className={cn(
        'num inline-flex items-center gap-1.5 text-[11px]',
        direction === 'long' ? 'text-long' : 'text-short',
      )}
    >
      <span className={cn('h-3 w-0.5 rounded-full', direction === 'long' ? 'bg-long' : 'bg-short')} />
      {unitLabel(unit)}
    </span>
  )
}

function SideText({ side }: { side: Side }) {
  return <span className={side === 'buy' ? 'text-long' : 'text-short'}>{SIDE_LABELS[side]}</span>
}

/** 数量：有成交时写成「已成交 / 总量」。 */
function FilledQuantity({ filled, quantity }: { filled: string; quantity: string }) {
  if (Number(filled) === 0) return <Private>{decimal(quantity)}</Private>
  return (
    <>
      <Private>{decimal(filled)}</Private>
      <span className="text-muted-foreground"> / <Private>{decimal(quantity)}</Private></span>
    </>
  )
}

/** 进度条：已完成的部分占多少。 */
function Meter({ part, whole, tone }: { part: string; whole: string; tone: 'long' | 'short' | 'signal' }) {
  const ratio = Number(whole) > 0 ? Math.min(Math.max(Number(part) / Number(whole), 0), 1) : 0
  return (
    <span className="inline-block h-1.5 w-14 overflow-hidden rounded-full bg-muted align-middle">
      <span
        className={cn(
          'block h-full rounded-full',
          tone === 'long' ? 'bg-long' : tone === 'short' ? 'bg-short' : 'bg-signal',
        )}
        style={{ width: `${ratio * 100}%` }}
      />
    </span>
  )
}

/**
 * 格子阶梯：所有格子按建仓价从高到低排成一列，最近的标记价作为一条亮线插在它所在的
 * 价位上。一眼看出价格走到了哪一格、上面的格子卖出等买回、下面的格子挂着等买入。
 */
export function CellLadder({
  cells,
  mark,
  profit = false,
}: {
  cells: CellSummary[]
  mark?: string | null
  /** 显示整格收益：预览时是决策依据，运行详情里不再需要。 */
  profit?: boolean
}) {
  if (cells.length === 0) return <Empty>没有网格格子：主仓比例是 1，只积累主仓。</Empty>
  const sorted = [...cells].sort((a, b) => Number(b.entry_price) - Number(a.entry_price))
  // 只有格子还没启用时才有状态可说；全都启用了就不要这一列。
  const statusColumn = sorted.some((cell) => CELL_STATUS_LABELS[cell.status] !== undefined)
  const columns = 6 + (statusColumn ? 1 : 0) + (profit ? 1 : 0)
  const markValue = Number(mark)
  const markAt =
    mark === null || mark === undefined || !Number.isFinite(markValue) || markValue <= 0
      ? -1
      : sorted.findIndex((cell) => Number(cell.entry_price) < markValue)
  const markRow = (
    <tr aria-hidden="true" className="!border-t-0">
      <td colSpan={columns} className="!h-0 !p-0">
        <div className="relative flex h-5 items-center pl-3">
          <span className="num mr-2 rounded bg-signal px-1.5 text-[10px] leading-4 font-semibold text-primary-foreground">
            标记价 {mark}
          </span>
          <div className="h-px flex-1 bg-signal/70 shadow-[0_0_6px] shadow-signal/50" />
        </div>
      </td>
    </tr>
  )
  return (
    <Table>
      <caption className="sr-only">网格格子表</caption>
      <thead>
        <tr>
          <th>格</th>
          <th>建仓</th>
          <th>平仓</th>
          <th>数量</th>
          <th>持有</th>
          <th title="在场挂单数">挂单</th>
          {statusColumn ? <th>状态</th> : null}
          {profit ? <th title="整格成交一次的扣费后收益：按 maker / 按 taker">整格收益</th> : null}
        </tr>
      </thead>
      <tbody>
        {sorted.map((cell, position) => (
          <LadderRow
            key={`${cell.direction}-${cell.index}`}
            cell={cell}
            profit={profit}
            statusColumn={statusColumn}
            markBefore={position === markAt ? markRow : null}
          />
        ))}
        {markAt === -1 && mark !== null && mark !== undefined && markValue > 0 ? markRow : null}
      </tbody>
    </Table>
  )
}

function LadderRow({
  cell,
  profit,
  statusColumn,
  markBefore,
}: {
  cell: CellSummary
  profit: boolean
  statusColumn: boolean
  markBefore: React.ReactNode
}) {
  const held = Number(cell.held) > 0
  const status = CELL_STATUS_LABELS[cell.status]
  return (
    <>
      {markBefore}
      <tr className={cn(held && (cell.direction === 'long' ? 'bg-long/5' : 'bg-short/5'))}>
        <td>
          <span className="inline-flex items-baseline gap-1">
            <UnitTag unit={{ cell: [cell.direction, cell.index] }} />
            {cell.batch === null ? null : (
              <span className="num text-[10px] text-muted-foreground">批{cell.batch}</span>
            )}
          </span>
        </td>
        <td className="num font-medium">{decimal(cell.entry_price)}</td>
        <td className="num text-muted-foreground">{decimal(cell.exit_price)}</td>
        <td className="num"><Private>{decimal(cell.quantity)}</Private></td>
        <td className="num">
          <span className="inline-flex items-center gap-2">
            <Meter part={cell.held} whole={cell.quantity} tone={cell.direction} />
            <span className={cn(!held && 'text-muted-foreground')}><Private>{decimal(cell.held)}</Private></span>
          </span>
        </td>
        <td className="num">
          {cell.working_orders === 0 ? (
            <span className="text-muted-foreground">—</span>
          ) : (
            <span className="text-signal">{cell.working_orders}</span>
          )}
        </td>
        {statusColumn ? <td>{status === undefined ? null : <Pill tone="muted">{status}</Pill>}</td> : null}
        {profit ? (
          <td className="num text-muted-foreground">
            <Private>{decimal(cell.profit_maker)}</Private>
            <span className="text-[10px]"> / <Private>{decimal(cell.profit_taker)}</Private></span>
          </td>
        ) : null}
      </tr>
    </>
  )
}

/**
 * 批次状态：还没放行的、已经放行的、已经结束的（设计第 11 节）。
 *
 * 服务端给的是「已买入多少、有几个格子已启用、是否结束」，状态由它们确定。
 */
function batchState(batch: BatchSummary): { label: string; tone: Tone } {
  if (batch.finished) return { label: '已结束', tone: 'muted' }
  if (batch.enabled_cell_count > 0 || Number(batch.core_filled) > 0) return { label: '已放行', tone: 'long' }
  return { label: '未放行', tone: 'info' }
}

export function BatchTable({ batches }: { batches: BatchSummary[] }) {
  if (batches.length === 0) return <Empty>没有主仓批次：主仓比例是 0。</Empty>
  return (
    <Table>
      <caption className="sr-only">主仓批次表</caption>
      <thead>
        <tr>
          <th>批次</th>
          <th>价格</th>
          <th>主仓</th>
          <th>网格</th>
          <th>格子</th>
          <th>状态</th>
          <th title="剩余不足最低下单金额而提前结束时，少建的主仓 / 网格数量">少建</th>
        </tr>
      </thead>
      <tbody>
        {batches.map((batch) => {
          const state = batchState(batch)
          return (
            <tr key={`${batch.direction}-${batch.index}`}>
              <td>
                <UnitTag unit={{ batch: [batch.direction, batch.index] }} />
              </td>
              <td className="num font-medium">{decimal(batch.price)}</td>
              <td className="num">
                <span className="inline-flex items-center gap-2">
                  <Meter part={batch.core_filled} whole={batch.core_quantity} tone="signal" />
                  <Private>{decimal(batch.core_filled)}</Private>
                  <span className="text-muted-foreground"> / <Private>{decimal(batch.core_quantity)}</Private></span>
                </span>
              </td>
              <td className="num text-muted-foreground"><Private>{decimal(batch.grid_quantity)}</Private></td>
              <td className="num">
                {batch.enabled_cell_count}
                <span className="text-muted-foreground"> / {batch.cell_count}</span>
              </td>
              <td>
                <Pill tone={state.tone}>{state.label}</Pill>
              </td>
              {/* 剩余不足最低下单金额而提前结束的批次，少建了多少主仓和网格（第 4.4 节，B05）。 */}
              <td className="num">
                {Number(batch.core_shortfall) === 0 && Number(batch.grid_shortfall) === 0 ? (
                  <span className="text-muted-foreground">—</span>
                ) : (
                  <span className="text-warn"><Private>{`${decimal(batch.core_shortfall)} / ${decimal(batch.grid_shortfall)}`}</Private></span>
                )}
              </td>
            </tr>
          )
        })}
      </tbody>
    </Table>
  )
}

const ORDER_TONE: Record<OrderStatus, Tone> = {
  submitting: 'info',
  open: 'long',
  partially_filled: 'signal',
  filled: 'muted',
  canceled: 'muted',
  rejected: 'danger',
  void: 'muted',
  unknown: 'warn',
}

export function OrdersTable({ orders }: { orders: StoredOrder[] }) {
  if (orders.length === 0) return <Empty>没有在场挂单。</Empty>
  // 和格子阶梯一样按价格从高到低。全都在场时不要状态列：每行都一样的字不用写。
  const sorted = [...orders].sort((a, b) => Number(b.record.limit_price) - Number(a.record.limit_price))
  const statusColumn = sorted.some((order) => order.record.status !== 'open')
  return (
    <Table>
      <caption className="sr-only">在场挂单</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th title="开：格子建仓；平：格子平仓">用途</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
          {statusColumn ? <th>状态</th> : null}
        </tr>
      </thead>
      <tbody>
        {sorted.map((order) => {
          const record = order.record
          return (
            <tr key={record.client_order_id} title={`到期 ${timestamp(order.expires_at)}`}>
              <td>
                <UnitTag unit={record.unit} />
              </td>
              <td>
                {PURPOSE_LABELS[record.purpose]}
                {record.reduce_only ? <span className="ml-1 text-[10px] text-muted-foreground">只减</span> : null}
              </td>
              <td>
                <SideText side={orderSide(unitDirection(record.unit), record.purpose)} />
              </td>
              <td className="num font-medium">{decimal(record.limit_price)}</td>
              <td className="num">
                <FilledQuantity filled={record.cumulative_filled} quantity={record.quantity} />
              </td>
              {statusColumn ? (
                <td>
                  {record.status === 'open' ? null : (
                    <Pill tone={ORDER_TONE[record.status]}>{ORDER_STATUS_LABELS[record.status]}</Pill>
                  )}
                </td>
              ) : null}
            </tr>
          )
        })}
      </tbody>
    </Table>
  )
}

/** 启动后会立即成交的订单：确认启动之前必须看到这些。 */
export function ImmediateFillTable({ fills }: { fills: ImmediateFill[] }) {
  if (fills.length === 0) return <Empty>启动时没有会立即成交的订单。</Empty>
  return (
    <Table>
      <caption className="sr-only">启动后立即成交</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th>用途</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
        </tr>
      </thead>
      <tbody>
        {fills.map((fill, index) => (
          <tr key={`${unitLabel(fill.unit)}-${fill.purpose}-${index}`}>
            <td>
              <UnitTag unit={fill.unit} />
            </td>
            <td>{PURPOSE_LABELS[fill.purpose]}</td>
            <td>
              <SideText side={orderSide(unitDirection(fill.unit), fill.purpose)} />
            </td>
            <td className="num font-medium">{decimal(fill.limit_price)}</td>
            <td className="num"><Private>{decimal(fill.quantity)}</Private></td>
          </tr>
        ))}
      </tbody>
    </Table>
  )
}

/** 启动后第一步会挂出的订单（第 8 节：接管已有仓位时，持有格子的平仓单和没买完的主仓单）。 */
export function PlannedOrderTable({ orders }: { orders: OrderIntent[] }) {
  if (orders.length === 0) return <Empty>启动后第一步不会挂出订单。</Empty>
  return (
    <Table>
      <caption className="sr-only">启动后第一步挂出的订单</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th>用途</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
        </tr>
      </thead>
      <tbody>
        {orders.map((order, index) => (
          <tr key={`${unitLabel(order.unit)}-${order.purpose}-${index}`}>
            <td>
              <UnitTag unit={order.unit} />
            </td>
            <td>
              {PURPOSE_LABELS[order.purpose]}
              {order.reduce_only ? <span className="ml-1 text-[10px] text-muted-foreground">只减</span> : null}
            </td>
            <td>
              <SideText side={order.side} />
            </td>
            <td className="num font-medium">{decimal(order.limit_price)}</td>
            <td className="num"><Private>{decimal(order.quantity)}</Private></td>
          </tr>
        ))}
      </tbody>
    </Table>
  )
}

/** 交易所上这个标的已有的挂单：不是本策略的，本系统不撤销，由用户自己处理（第 8 节）。 */
export function ForeignOrderTable({ orders }: { orders: ExchangeOrder[] }) {
  return (
    <Table>
      <caption className="sr-only">交易所上已有的其他挂单（本系统不撤销）</caption>
      <thead>
        <tr>
          <th>订单</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
        </tr>
      </thead>
      <tbody>
        {orders.map((order) => (
          <tr key={order.exchange_order_id}>
            <td className="num">{order.client_order_id ?? `${order.exchange_order_id}（手动下单）`}</td>
            <td>
              <SideText side={order.side} />
            </td>
            <td className="num">{decimal(order.limit_price)}</td>
            <td className="num">
              <FilledQuantity filled={order.cumulative_filled} quantity={order.quantity} />
            </td>
          </tr>
        ))}
      </tbody>
    </Table>
  )
}

function liquidationText(liquidation: Liquidation): string {
  switch (liquidation.kind) {
    case 'never':
      return '不会强平'
    case 'at':
      return decimal(liquidation.price)
    case 'out_of_range':
      return '超出计算范围'
  }
}

/** 满仓压力测试（第 7.3 节）：每一侧满仓、价格到最不利位置时的账户保证金率和强平价。 */
export function StressTable({ sides }: { sides: SideStress[] }) {
  if (sides.length === 0) return null
  return (
    <Table>
      <caption className="sr-only">满仓压力测试</caption>
      <thead>
        <tr>
          <th>方向</th>
          <th>最不利价</th>
          <th>满仓</th>
          <th>均价</th>
          <th>保证金率</th>
          <th>强平价</th>
        </tr>
      </thead>
      <tbody>
        {sides.map((side) => (
          <tr key={side.direction}>
            <td>
              <DirectionTag direction={side.direction} />
            </td>
            <td className="num">{decimal(side.adverse_price)}</td>
            <td className="num"><Private>{decimal(side.position)}</Private></td>
            <td className="num">{decimal(side.average_entry)}</td>
            <td className="num font-medium">{percent(side.margin_ratio)}</td>
            <td className={cn('num', side.liquidation.kind === 'at' && 'text-destructive')}>
              {liquidationText(side.liquidation)}
            </td>
          </tr>
        ))}
      </tbody>
    </Table>
  )
}

function intervalText(interval: ThrottleInterval): string {
  return interval === null ? '不限速' : `${interval[0]} 秒`
}

/** 每一侧的限速间隔各说一遍：中性网格两侧各有一条限速链。 */
function throttleText(config: StrategyConfig): string {
  const parts: string[] = []
  if (config.long !== null) parts.push(`做多 ${intervalText(config.long.throttle_interval)}`)
  if (config.short !== null) parts.push(`做空 ${intervalText(config.short.throttle_interval)}`)
  return parts.join('；')
}

export function ConfigVersionTable({ versions }: { versions: StoredConfigVersion[] }) {
  const { hidden } = usePrivacy()
  return (
    <Table>
      <caption className="sr-only">参数版本</caption>
      <thead>
        <tr>
          <th>版本</th>
          <th>时间</th>
          <th>限速</th>
          <th className="!text-left">参数</th>
        </tr>
      </thead>
      <tbody>
        {[...versions].reverse().map((version) => (
          <tr key={version.version}>
            <td className="num">v{version.version}</td>
            <td className="num text-muted-foreground">{timestamp(version.recorded_at)}</td>
            <td className="text-muted-foreground">{throttleText(version.config)}</td>
            <td className="!text-left !whitespace-normal text-muted-foreground">{configSummary(version.config, hidden)}</td>
          </tr>
        ))}
      </tbody>
    </Table>
  )
}
