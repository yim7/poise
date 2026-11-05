// 填写时的仓位划分（设计第 2.4 节第 1 步）：每一侧设置卡片底部的一块，边填边看算出来的
// M、资金怎么分给主仓和网格、价格走到哪会持有多少。
//
// 数量、价格、金额都来自服务端（`/api/sizing`），这里只把它们换成图上的位置，以及按价位
// 把批次和格子加起来。

import { useState } from 'react'

import { Private, PrivateText } from '@/components/privacy'
import { tint } from '@/components/range-bar'
import { cn } from '@/lib/utils'
import { usdc } from './format'
import { DIRECTION_LABELS } from './labels'
import type {
  BatchSummary,
  CellSummary,
  Direction,
  ImmediateFill,
  SideConfig,
  SideSizing,
  SizingPreview,
} from './types'

/**
 * 编辑页上划分的状态：参数不全、算不出（原因）、算好了。`stale` 是参数已经改了、新结果
 * 还没回来：旧数字照常显示但压暗，免得和刚改的输入框对不上。
 */
export type Breakdown = (
  | { kind: 'incomplete' }
  | { kind: 'problem'; message: string }
  | { kind: 'ready'; preview: SizingPreview; long: SideConfig | null; short: SideConfig | null }
) & { stale?: boolean }

/** 主仓的实心色：同侧颜色取 `percent`% 的不透明度。 */
function solid(direction: Direction, percent: number): string {
  return `color-mix(in oklch, var(--${direction}) ${percent}%, transparent)`
}

/** 一个价位上已经成交的部分：哪几批主仓、哪几格网格，以及按挂单价花了多少。 */
interface Holding {
  core: number
  grid: number
  batches: number
  cells: number
  spent: number
  /** 成交了的单位，`b1`、`c7` 这样记：对照启动时会成交的单用。 */
  units: string[]
}

function holdingAt(direction: Direction, price: number, batches: BatchSummary[], cells: CellSummary[]): Holding {
  // 做多：价格跌到某个价位时，挂在这个价位及以上的买单都成交了；做空反过来。
  const reached = (at: number) => (direction === 'long' ? at >= price : at <= price)
  const holding: Holding = { core: 0, grid: 0, batches: 0, cells: 0, spent: 0, units: [] }
  for (const batch of batches) {
    const at = Number(batch.price)
    if (!reached(at)) continue
    holding.core += Number(batch.core_quantity)
    holding.batches += 1
    holding.spent += at * Number(batch.core_quantity)
    holding.units.push(`b${batch.index}`)
  }
  for (const cell of cells) {
    const at = Number(cell.entry_price)
    if (!reached(at)) continue
    holding.grid += Number(cell.quantity)
    holding.cells += 1
    holding.spent += at * Number(cell.quantity)
    holding.units.push(`c${cell.index}`)
  }
  return holding
}

/**
 * 按现价启动时第一步就成交的部分，来自服务端算好的立即成交单：设了限速时只有放行的那一个
 * 单位，不能按「现价以上的都买到」来估。立即成交的单按约现价成交，金额按现价算。
 */
function holdingAtStart(direction: Direction, fills: ImmediateFill[], marketPrice: number): Holding {
  const holding: Holding = { core: 0, grid: 0, batches: 0, cells: 0, spent: 0, units: [] }
  for (const fill of fills) {
    const quantity = Number(fill.quantity)
    if ('batch' in fill.unit && fill.unit.batch[0] === direction) {
      holding.core += quantity
      holding.batches += 1
      holding.units.push(`b${fill.unit.batch[1]}`)
    } else if ('cell' in fill.unit && fill.unit.cell[0] === direction) {
      holding.grid += quantity
      holding.cells += 1
      holding.units.push(`c${fill.unit.cell[1]}`)
    } else {
      continue
    }
    holding.spent += marketPrice * quantity
  }
  return holding
}

/** 占比到 0.1%；有但不到 0.1% 的写「<0.1%」，不写成 0.0%。 */
function share(ratio: number): string {
  const percent = ratio * 100
  return percent > 0 && percent < 0.05 ? '<0.1%' : `${percent.toFixed(1)}%`
}

/** 数量按交易所给的最细小数位显示：合计是浮点加出来的，要收回到同一精度。 */
function places(values: string[]): number {
  return values.reduce((most, value) => Math.max(most, value.split('.')[1]?.length ?? 0), 0)
}

/** 价格显示：整数价不带小数，其余最多保留到给出的位数。 */
function priceText(value: number, digits: number): string {
  return Number.isInteger(value) ? String(value) : value.toFixed(digits)
}

const WIDTH = 360
const TOP = 6
const BOTTOM = 100
const HEIGHT = 116

export function SideBreakdown({ direction, breakdown }: { direction: Direction; breakdown: Breakdown | null }) {
  const label = DIRECTION_LABELS[direction]
  return (
    <section
      aria-label={`${label}侧仓位划分`}
      aria-busy={breakdown?.stale === true}
      className={cn(
        'flex flex-col gap-3 rounded-md bg-background/60 p-3 ring-1 ring-border transition-opacity',
        breakdown?.stale === true && 'opacity-60',
      )}
    >
      <Body direction={direction} breakdown={breakdown} />
    </section>
  )
}

function Header({ marketPrice, stale = false }: { marketPrice?: string; stale?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="font-heading text-xs font-semibold tracking-tight">
        仓位划分
        {stale ? <span className="ml-2 text-[11px] font-normal text-muted-foreground">更新中…</span> : null}
      </span>
      {marketPrice === undefined ? null : (
        <span className="text-[11px] text-muted-foreground">
          现价 <span className="num text-signal">{marketPrice}</span>
        </span>
      )}
    </div>
  )
}

function Note({ children, tone = 'muted' }: { children: React.ReactNode; tone?: 'muted' | 'warn' }) {
  return (
    <p
      className={cn(
        'text-xs/relaxed',
        tone === 'warn' ? 'rounded-sm border-l-[3px] border-warn bg-warn/8 px-2 py-1.5 text-foreground/85' : 'text-muted-foreground',
      )}
    >
      {children}
    </p>
  )
}

/**
 * 一句原因和这一侧有没有关系：服务端和表单的原因都以「做多侧」「做空侧」开头；以另一侧开头、
 * 又没提到这一侧的才与它无关。「做多侧上限 H 须低于做空侧下限 L」这种两侧都要看。
 */
function concerns(direction: Direction, message: string): boolean {
  const other: Direction = direction === 'long' ? 'short' : 'long'
  return !message.startsWith(`${DIRECTION_LABELS[other]}侧`) || message.includes(`${DIRECTION_LABELS[direction]}侧`)
}

function Body({ direction, breakdown }: { direction: Direction; breakdown: Breakdown | null }) {
  if (breakdown === null) {
    return (
      <>
        <Header />
        <Note>计算中…</Note>
      </>
    )
  }
  const stale = breakdown.stale === true
  if (breakdown.kind === 'incomplete') {
    return (
      <>
        <Header stale={stale} />
        <Note>填完各项数字后显示划分。</Note>
      </>
    )
  }
  const reason =
    breakdown.kind === 'problem'
      ? breakdown.message
      : breakdown.preview.validation.valid
        ? null
        : (breakdown.preview.validation.reason ?? '参数不成立。')
  if (reason !== null) {
    const marketPrice = breakdown.kind === 'ready' ? breakdown.preview.market_price : undefined
    return (
      <>
        <Header marketPrice={marketPrice} stale={stale} />
        {concerns(direction, reason) ? (
          <Note tone="warn">
            <span className="block font-medium text-warn">还算不出划分</span>
            <PrivateText>{reason}</PrivateText>
          </Note>
        ) : (
          <Note>另一侧的参数还不成立，改好后一起显示。</Note>
        )}
      </>
    )
  }
  if (breakdown.kind !== 'ready') return null
  const { preview } = breakdown
  const sizing = preview.sizing.find((side) => side.direction === direction)
  const config = breakdown[direction]
  if (sizing === undefined || config === null) {
    return (
      <>
        <Header marketPrice={preview.market_price} />
        <Note>计算中…</Note>
      </>
    )
  }
  return (
    <>
      <Header marketPrice={preview.market_price} stale={stale} />
      <Ready
        direction={direction}
        sizing={sizing}
        config={config}
        marketPrice={preview.market_price}
        leverage={preview.leverage}
        immediateFills={preview.immediate_fills}
        batches={preview.batches.filter((batch) => batch.direction === direction)}
        cells={preview.cells.filter((cell) => cell.direction === direction)}
      />
    </>
  )
}

interface ReadyProps {
  direction: Direction
  sizing: SideSizing
  config: SideConfig
  marketPrice: string
  leverage: number | null
  immediateFills: ImmediateFill[]
  batches: BatchSummary[]
  cells: CellSummary[]
}

function Ready(props: ReadyProps) {
  const { direction, sizing, config, leverage } = props
  const capital = config.capital === null ? null : Number(config.capital)
  return (
    <>
      <div className="grid grid-cols-3 gap-2">
        <Reading label="最大仓位 M" value={<Private>{sizing.max_position}</Private>} />
        <Reading
          label={capital === null ? '满仓总价值' : '用到 / 投入'}
          value={
            <>
              <Private>{usdc(sizing.notional)}</Private>
              {capital === null ? null : (
                <span className="text-[11px] text-muted-foreground">
                  {' '}/ <Private>{usdc(config.capital ?? '0')}</Private>
                </span>
              )}
            </>
          }
        />
        <Reading
          label={leverage === null ? '保证金占用' : `保证金 · ${leverage}x`}
          value={leverage === null ? '读不到杠杆' : <Private>{usdc(sizing.margin)}</Private>}
        />
      </div>
      <Split direction={direction} sizing={sizing} capital={capital} />
      <Ladder {...props} />
    </>
  )
}

function Reading({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className="num text-sm break-all">{value}</span>
    </div>
  )
}

interface Segment {
  label: string
  amount: number
  color: string
  outline?: string
}

/** 资金拆分条：主仓（失效后保留的、失效时卖出的）、网格，按资金设置时还有没用到的零头。 */
function Split({ direction, sizing, capital }: { direction: Direction; sizing: SideSizing; capital: number | null }) {
  const notional = Number(sizing.notional)
  const core = Number(sizing.core_notional)
  const grid = notional - core
  const coreQuantity = Number(sizing.core_quantity)
  const remaining = sizing.invalidation_remaining === null ? null : Number(sizing.invalidation_remaining)
  // 失效后留下的那部分主仓按数量占比折成金额：留下的是哪几批不重要，看的是大头在哪。
  const kept = remaining !== null && coreQuantity > 0 ? (core * remaining) / coreQuantity : 0
  const segments: Segment[] = []
  if (kept > 0) segments.push({ label: '主仓 · 失效后保留', amount: kept, color: solid(direction, 55) })
  if (core - kept > 0) {
    segments.push({
      label: remaining === null ? '主仓' : '主仓 · 失效时卖出',
      amount: core - kept,
      color: solid(direction, kept > 0 ? 28 : 45),
    })
  }
  if (grid > 0) {
    segments.push({
      label: `网格 · ${sizing.cell_count} 格`,
      amount: grid,
      color: tint(direction, 14),
      outline: tint(direction, 80),
    })
  }
  const total = capital !== null && capital > notional ? capital : notional
  if (total > notional) {
    segments.push({ label: '未用', amount: total - notional, color: 'transparent', outline: 'var(--border)' })
  }
  if (total <= 0) return null
  return (
    <div className="flex flex-col gap-2">
      <div className="flex h-4 gap-0.5" aria-hidden>
        {segments.map((segment) => (
          <span
            key={segment.label}
            className="min-w-[3px] rounded-[2px]"
            style={{
              flexGrow: segment.amount,
              flexBasis: 0,
              background: segment.color,
              boxShadow: segment.outline === undefined ? undefined : `inset 0 0 0 1px ${segment.outline}`,
            }}
          />
        ))}
      </div>
      <ul aria-label="资金拆分" className="grid grid-cols-2 gap-x-3 gap-y-1.5 sm:grid-cols-3">
        {segments.map((segment) => (
          <li key={segment.label} className="flex min-w-0 flex-col">
            <span className="flex items-center gap-1 text-[11px] text-muted-foreground">
              <span
                className="size-2 shrink-0 rounded-[2px]"
                style={{
                  background: segment.color,
                  boxShadow: segment.outline === undefined ? undefined : `inset 0 0 0 1px ${segment.outline}`,
                }}
              />
              {segment.label}
            </span>
            <span className="num text-xs">
              <Private>{segment.amount.toFixed(2)}</Private>
              <span className="text-muted-foreground"> {share(segment.amount / total)}</span>
            </span>
          </li>
        ))}
      </ul>
    </div>
  )
}

/** 阶梯上的一段：这段价格里持有多少，以及它是不是启动时就会成交的那一边。 */
interface Column {
  low: number
  high: number
  holding: Holding
  crossed: boolean
}

/**
 * 持仓阶梯：横轴价格，竖轴持仓。做多时价格一路跌到哪里，就持有哪里及以上挂的主仓和网格；
 * 做空镜像。反弹时网格卖回、主仓留下，所以主仓画实心、网格画成和价格条一样的灯管。
 */
function Ladder({ direction, sizing, config, marketPrice: marketText, immediateFills, batches, cells }: ReadyProps) {
  const [hover, setHover] = useState<number | null>(null)
  const marketPrice = Number(marketText)
  const low = Number(config.price_low)
  const high = Number(config.price_high)
  const invalidation = config.invalidation === null ? null : Number(config.invalidation.price)
  const remaining = sizing.invalidation_remaining === null ? 0 : Number(sizing.invalidation_remaining)
  const quantityPlaces = places([
    sizing.max_position,
    ...batches.map((batch) => batch.core_quantity),
    ...cells.map((cell) => cell.quantity),
  ])
  const pricePlaces = places([
    config.price_low,
    config.price_high,
    marketText,
    ...(config.invalidation === null ? [] : [config.invalidation.price]),
    ...batches.map((batch) => batch.price),
    ...cells.map((cell) => cell.entry_price),
  ])

  const outer = invalidation ?? (direction === 'long' ? low : high)
  const floor = Math.min(low, outer)
  const ceiling = Math.max(high, outer)
  const span = ceiling - floor || 1
  // 失效价外面留一段画「失效后剩余 R」。
  const beyond = invalidation === null ? span * 0.03 : span * 0.08
  const from = direction === 'long' ? floor - beyond : floor - span * 0.03
  const to = direction === 'long' ? ceiling + span * 0.03 : ceiling + beyond
  const x = (price: number) => ((price - from) / (to - from)) * WIDTH

  const atStart = holdingAtStart(direction, immediateFills, marketPrice)
  const started = new Set(atStart.units)

  // 轴的两头也是分界：没设失效价时，最外一段是满仓，要画出来，不能留成空白像是 0。
  // 设了失效价时，失效价外面那段画剩余 R，不在这里画。
  const breaks = [
    ...new Set([
      low,
      high,
      ...(invalidation === null ? [from, to] : direction === 'long' ? [invalidation, to] : [from, invalidation]),
      ...batches.map((batch) => Number(batch.price)),
      ...cells.map((cell) => Number(cell.entry_price)),
    ]),
  ].sort((a, b) => a - b)
  const columns: Column[] = []
  for (let index = 0; index + 1 < breaks.length; index += 1) {
    const [a, b] = [breaks[index], breaks[index + 1]]
    // 做多在 (a, b) 之间持有的是挂在 b 及以上的单，做空是挂在 a 及以下的单。
    const holding = holdingAt(direction, direction === 'long' ? b : a, batches, cells)
    // 这一段要持有的单启动时全都会成交，才画亮：限速时只有放行的那个单位会成交。
    const crossed = holding.units.length > 0 && holding.units.every((unit) => started.has(unit))
    columns.push({ low: a, high: b, holding, crossed })
  }
  const peak = Math.max(Number(sizing.max_position), ...columns.map((column) => column.holding.core + column.holding.grid))
  const y = (quantity: number) => BOTTOM - (quantity / (peak * 1.08 || 1)) * (BOTTOM - TOP)

  const beyondInvalidation = (price: number) =>
    invalidation !== null && (direction === 'long' ? price < invalidation : price > invalidation)
  const at = hover ?? marketPrice
  const atHolding = hover === null ? atStart : holdingAt(direction, hover, batches, cells)

  function track(event: React.MouseEvent<SVGSVGElement>) {
    const box = event.currentTarget.getBoundingClientRect()
    if (box.width <= 0) return
    const ratio = (event.clientX - box.left) / box.width
    setHover(from + Math.min(Math.max(ratio, 0), 1) * (to - from))
  }

  // 轴上的价格：现价最要紧，其次失效价，再是范围两端；挨得太近的后来者不写。
  const ticks: { price: number; color: string }[] = []
  const candidates = [
    { price: marketPrice, color: 'var(--signal)' },
    ...(invalidation === null ? [] : [{ price: invalidation, color: 'var(--destructive)' }]),
    { price: low, color: 'var(--muted-foreground)' },
    { price: high, color: 'var(--muted-foreground)' },
  ]
  for (const tick of candidates) {
    if (tick.price < from || tick.price > to) continue
    if (ticks.some((placed) => Math.abs(x(placed.price) - x(tick.price)) < 44)) continue
    ticks.push(tick)
  }

  const markInside = marketPrice >= from && marketPrice <= to
  return (
    <div className="flex flex-col gap-1.5">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="w-full"
        role="img"
        aria-label={`${DIRECTION_LABELS[direction]}侧持仓阶梯`}
        onMouseMove={track}
        onMouseLeave={() => setHover(null)}
      >
        <line x1={0} x2={WIDTH} y1={BOTTOM} y2={BOTTOM} stroke="var(--border)" />
        {columns.map((column) => {
          const left = x(column.low) + 0.5
          const width = Math.max(x(column.high) - x(column.low) - 1, 0.5)
          const { core, grid } = column.holding
          return (
            <g key={column.low}>
              {core > 0 ? (
                <rect
                  data-part="core"
                  x={left}
                  y={y(core)}
                  width={width}
                  height={BOTTOM - y(core)}
                  style={{ fill: solid(direction, column.crossed ? 55 : 30) }}
                />
              ) : null}
              {grid > 0 ? (
                <rect
                  data-part="grid"
                  x={left}
                  y={y(core + grid)}
                  width={width}
                  height={Math.max(y(core) - y(core + grid) - 0.5, 0.5)}
                  rx={1}
                  style={{
                    fill: tint(direction, column.crossed ? 22 : 4),
                    stroke: tint(direction, column.crossed ? 90 : 28),
                    strokeWidth: 0.8,
                  }}
                />
              ) : null}
            </g>
          )
        })}
        {invalidation === null ? null : (
          <>
            {remaining > 0 ? (
              <rect
                data-part="remaining"
                x={direction === 'long' ? 0.5 : x(invalidation) + 0.5}
                y={y(remaining)}
                width={direction === 'long' ? x(invalidation) - 1 : WIDTH - x(invalidation) - 1}
                height={BOTTOM - y(remaining)}
                style={{ fill: solid(direction, 28), stroke: tint(direction, 50), strokeDasharray: '2 2' }}
              />
            ) : null}
            <line
              x1={x(invalidation)}
              x2={x(invalidation)}
              y1={TOP}
              y2={BOTTOM}
              stroke="var(--destructive)"
              strokeDasharray="2 2"
            />
          </>
        )}
        {markInside ? (
          <line x1={x(marketPrice)} x2={x(marketPrice)} y1={TOP} y2={BOTTOM} stroke="var(--signal)" strokeWidth={1.5} />
        ) : null}
        {hover === null ? null : (
          <line x1={x(hover)} x2={x(hover)} y1={TOP} y2={BOTTOM} stroke="var(--muted-foreground)" opacity={0.7} />
        )}
        {ticks.map((tick) => {
          const position = x(tick.price)
          const anchor = position < 24 ? 'start' : position > WIDTH - 24 ? 'end' : 'middle'
          return (
            <text
              key={`${tick.color}-${tick.price}`}
              x={position}
              y={HEIGHT - 3}
              textAnchor={anchor}
              className="num"
              style={{ fill: tick.color, fontSize: 9.5 }}
            >
              {priceText(tick.price, pricePlaces)}
            </text>
          )
        })}
      </svg>
      <p className="num text-[11px] text-muted-foreground" aria-live="polite">
        {beyondInvalidation(at) ? (
          <span className="text-destructive">
            越过失效价 {priceText(invalidation as number, pricePlaces)}：主仓减到{' '}
            <Private>{sizing.invalidation_remaining ?? '0'}</Private>，网格全平，策略停止
          </span>
        ) : (
          <>
            {hover === null ? (
              <>
                <span className="text-foreground">现价</span> 启动即持有{' '}
              </>
            ) : (
              <>
                {direction === 'long' ? '一路跌到' : '一路涨到'}{' '}
                <span className="text-foreground">{priceText(at, pricePlaces)}</span> 持有{' '}
              </>
            )}
            <span className={direction === 'long' ? 'text-long' : 'text-short'}>
              <Private>{(atHolding.core + atHolding.grid).toFixed(quantityPlaces)}</Private>
            </span>
            {/* 只有主仓或只有网格时不写另一半的「0 批」「0 格」。 */}
            {batches.length > 0 && cells.length > 0 ? (
              <>
                {' '}= 主仓 {atHolding.batches} 批 <Private>{atHolding.core.toFixed(quantityPlaces)}</Private> + 网格{' '}
                {atHolding.cells} 格 <Private>{atHolding.grid.toFixed(quantityPlaces)}</Private>
              </>
            ) : batches.length > 0 ? (
              <>（主仓 {atHolding.batches} 批）</>
            ) : (
              <>（网格 {atHolding.cells} 格）</>
            )}
            ，{hover === null ? '约投入' : '已投入'} <Private>{atHolding.spent.toFixed(2)}</Private>
          </>
        )}
      </p>
    </div>
  )
}
