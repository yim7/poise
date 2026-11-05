// 填写时的仓位划分（设计第 2.4 节第 1 步）：每一侧设置卡片底部的一块，边填边看算出来的
// M、资金怎么分给主仓和网格、价格走到哪会持有多少。
//
// 数量、价格、金额都来自服务端（`/api/sizing`），这里只把它们换成图上的位置，以及按价位
// 把批次和格子加起来。

import { useState } from 'react'

import { ModeToggle } from '@/components/mode-toggle'
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

/** 划分的两种看法：挂单在价格上怎么排（默认），价格一路走过去持有多少。 */
type View = 'orders' | 'holding'

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
  // 看法记在这一层：参数改到一半算不出来、再算出来时，不跳回默认的那一种。
  const [view, setView] = useState<View>('orders')
  return (
    <section
      aria-label={`${label}侧仓位划分`}
      aria-busy={breakdown?.stale === true}
      className={cn(
        'flex flex-col gap-3 rounded-md bg-background/60 p-3 ring-1 ring-border transition-opacity',
        breakdown?.stale === true && 'opacity-60',
      )}
    >
      <Body direction={direction} breakdown={breakdown} view={view} onView={setView} />
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

function Body({
  direction,
  breakdown,
  view,
  onView,
}: {
  direction: Direction
  breakdown: Breakdown | null
  view: View
  onView: (view: View) => void
}) {
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
        view={view}
        onView={onView}
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

function Ready({ view, onView, ...props }: ReadyProps & { view: View; onView: (view: View) => void }) {
  const { direction, sizing, config, leverage, cells } = props
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
      {cells.length > 0 ? (
        <GridStats sizing={sizing} config={config} cells={cells} />
      ) : (
        <Note>只积累主仓，没有网格。</Note>
      )}
      <div className="flex flex-col gap-1.5">
        <div className="flex items-center justify-between gap-2">
          <span className="text-[11px] text-muted-foreground">
            {view === 'orders' ? '挂单在价格上怎么排' : '价格一路走过去持有多少'}
          </span>
          <ModeToggle
            label={`${DIRECTION_LABELS[direction]}侧划分看法`}
            value={view}
            options={[
              ['orders', '挂单排布'],
              ['holding', '持仓累计'],
            ]}
            disabled={false}
            onChange={onView}
          />
        </div>
        {view === 'orders' ? <OrderLayout {...props} /> : <Ladder {...props} />}
      </div>
    </>
  )
}

function Reading({
  label,
  value,
  hint,
  wrap = 'anywhere',
}: {
  label: string
  value: React.ReactNode
  hint?: React.ReactNode
  /** 放不下时在哪里折行：单个数字任意处，范围在破折号后面。 */
  wrap?: 'anywhere' | 'words'
}) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className={cn('num text-sm', wrap === 'anywhere' ? 'break-all' : 'break-words')}>{value}</span>
      {hint === undefined ? null : <span className="num text-[11px] text-muted-foreground">{hint}</span>}
    </div>
  )
}

/**
 * 一组数的范围：显示出来一样就写一个，不一样写「小–大」。破折号后面可以折行，
 * 免得数字从中间断开。
 */
function span(values: number[], format: (value: number) => string): string {
  const low = format(Math.min(...values))
  const high = format(Math.max(...values))
  return low === high ? low : `${low}–\u200b${high}`
}

/** 占比留两位有效数字：1.2%、0.24%、0.0012%，间距再小也不写成 0.00%。 */
function percentOf(ratio: number): string {
  return `${Number(ratio.toPrecision(2))}%`
}

/**
 * 网格有多细：格数、间距（也写成占价格的百分比，不熟悉的标的看比例更直观）、每格数量和
 * 金额、每格一买一卖扣掉挂单手续费后赚多少。
 */
function GridStats({ sizing, config, cells }: { sizing: SideSizing; config: SideConfig; cells: CellSummary[] }) {
  const spacing = Number(sizing.grid_spacing)
  const middle = (Number(config.price_low) + Number(config.price_high)) / 2
  const ratio = middle > 0 ? (spacing / middle) * 100 : 0
  const quantityPlaces = places(cells.map((cell) => cell.quantity))
  const quantities = cells.map((cell) => Number(cell.quantity))
  const amounts = cells.map((cell) => Number(cell.entry_price) * Number(cell.quantity))
  const profits = cells.map((cell) => Number(cell.profit_maker))
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
      <Reading label="网格" value={`${cells.length} 格`} hint={`价差 g ${sizing.grid_spread}`} />
      <Reading
        label="间距 d"
        value={sizing.grid_spacing}
        hint={`≈ ${percentOf(ratio)}`}
      />
      <Reading
        label="每格"
        wrap="words"
        value={<Private>{span(quantities, (value) => value.toFixed(quantityPlaces))}</Private>}
        hint={
          <>
            ≈ <Private>{span(amounts, (value) => value.toFixed(0))}</Private> USDC
          </>
        }
      />
      <Reading
        label="每轮扣费后"
        wrap="words"
        value={
          <span className="text-long">
            +<Private>{span(profits, (value) => value.toFixed(2))}</Private>
          </span>
        }
        hint="一买一卖都挂单成交"
      />
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

/** 两张图共用的价格轴：范围、失效价外留的一段、现价，以及轴上写哪几个价格。 */
interface Axis {
  low: number
  high: number
  invalidation: number | null
  marketPrice: number
  from: number
  to: number
  x: (price: number) => number
  pricePlaces: number
  ticks: { price: number; color: string }[]
}

function axisFor(
  direction: Direction,
  config: SideConfig,
  marketText: string,
  batches: BatchSummary[],
  cells: CellSummary[],
): Axis {
  const marketPrice = Number(marketText)
  const low = Number(config.price_low)
  const high = Number(config.price_high)
  const invalidation = config.invalidation === null ? null : Number(config.invalidation.price)
  const pricePlaces = places([
    config.price_low,
    config.price_high,
    marketText,
    ...(config.invalidation === null ? [] : [config.invalidation.price]),
    ...batches.map((batch) => batch.price),
    ...cells.flatMap((cell) => [cell.entry_price, cell.exit_price]),
  ])
  const outer = invalidation ?? (direction === 'long' ? low : high)
  // 最外面那格的平仓价在范围外面（做多是 H + g，做空是 L − g），轴要把它也包进来。
  const exits = cells.map((cell) => Number(cell.exit_price))
  const floor = Math.min(low, outer, ...exits)
  const ceiling = Math.max(high, outer, ...exits)
  const span = ceiling - floor || 1
  // 失效价外面留一段画「失效后剩余 R」。
  const beyond = invalidation === null ? span * 0.03 : span * 0.08
  const from = direction === 'long' ? floor - beyond : floor - span * 0.03
  const to = direction === 'long' ? ceiling + span * 0.03 : ceiling + beyond
  const x = (price: number) => ((price - from) / (to - from)) * WIDTH

  // 轴上的价格：现价最要紧，其次失效价，再是范围两端；挨得太近的后来者不写。
  const ticks: Axis['ticks'] = []
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
  return { low, high, invalidation, marketPrice, from, to, x, pricePlaces, ticks }
}

/** 失效价、现价两条竖线和轴上的价格，两张图画法一样。 */
function AxisMarks({ axis, top, bottom }: { axis: Axis; top: number; bottom: number }) {
  const { x, invalidation, marketPrice, from, to } = axis
  return (
    <>
      {invalidation === null ? null : (
        <line x1={x(invalidation)} x2={x(invalidation)} y1={top} y2={bottom} stroke="var(--destructive)" strokeDasharray="2 2" />
      )}
      {marketPrice >= from && marketPrice <= to ? (
        <line x1={x(marketPrice)} x2={x(marketPrice)} y1={top} y2={bottom} stroke="var(--signal)" strokeWidth={1.5} />
      ) : null}
      {axis.ticks.map((tick) => {
        const position = x(tick.price)
        const anchor = position < 24 ? 'start' : position > WIDTH - 24 ? 'end' : 'middle'
        return (
          <text
            key={`${tick.color}-${tick.price}`}
            x={position}
            y={bottom + 13}
            textAnchor={anchor}
            className="num"
            style={{ fill: tick.color, fontSize: 9.5 }}
          >
            {priceText(tick.price, axis.pricePlaces)}
          </text>
        )
      })}
    </>
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
  const axis = axisFor(direction, config, marketText, batches, cells)
  const { low, high, invalidation, marketPrice, from, to, x, pricePlaces } = axis
  const remaining = sizing.invalidation_remaining === null ? 0 : Number(sizing.invalidation_remaining)
  const quantityPlaces = places([
    sizing.max_position,
    ...batches.map((batch) => batch.core_quantity),
    ...cells.map((cell) => cell.quantity),
  ])

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
          </>
        )}
        {hover === null ? null : (
          <line x1={x(hover)} x2={x(hover)} y1={TOP} y2={BOTTOM} stroke="var(--muted-foreground)" opacity={0.7} />
        )}
        <AxisMarks axis={axis} top={TOP} bottom={BOTTOM} />
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

/** 挂单排布图里鼠标停着的那一张单。 */
type Pick = { kind: 'batch'; index: number } | { kind: 'cell'; index: number }

/** 一侧格子超过这么多，图上就不画每格的平仓价，格子之间也不留缝：画了也挤成一片。 */
const DENSE = 60

/**
 * 挂单排布：横轴价格，和价格条对齐。上排是主仓的建仓单，每批一根，挂在批次价上；下排是
 * 网格，每格一个灯管，宽度就是它占的那段价格，旁边一个空心点是它的平仓价，两者之间就是价差 g。
 * 主仓和网格各用各的高度：主仓一批常常是一格的十倍，放在一起格子就看不见了。启动时就会成交
 * 的单画亮，限速时只有放行的那个单位。
 */
function OrderLayout({ direction, sizing, config, marketPrice: marketText, immediateFills, batches, cells }: ReadyProps) {
  const [pick, setPick] = useState<Pick | null>(null)
  const axis = axisFor(direction, config, marketText, batches, cells)
  const { x, low, high } = axis
  const started = new Set(holdingAtStart(direction, immediateFills, axis.marketPrice).units)
  const quantityPlaces = places([...batches.map((batch) => batch.core_quantity), ...cells.map((cell) => cell.quantity)])
  const spacing = Number(sizing.grid_spacing)
  const dense = cells.length > DENSE
  const buy = direction === 'long' ? '买' : '卖'
  const sell = direction === 'long' ? '卖' : '买'

  // 两排的位置：只有主仓或只有网格时只画一排。
  let cursor = 0
  const coreLane = batches.length > 0 ? { label: cursor + 9, top: cursor + 14, bottom: cursor + 48 } : null
  if (coreLane !== null) cursor = coreLane.bottom + 8
  const gridLane = cells.length > 0 ? { label: cursor + 9, top: cursor + 14, bottom: cursor + 62 } : null
  if (gridLane !== null) cursor = gridLane.bottom
  const top = coreLane?.top ?? gridLane?.top ?? 0
  const exitY = (gridLane?.top ?? 0) + 4
  const bottom = cursor
  const height = bottom + 18

  const maxCore = Math.max(...batches.map((batch) => Number(batch.core_quantity)), 0)
  const batchXs = batches.map((batch) => x(Number(batch.price))).sort((a, b) => a - b)
  const batchGap = batchXs.slice(1).reduce((least, at, index) => Math.min(least, at - batchXs[index]), Infinity)
  const barWidth = Math.min(Math.max(batchGap * 0.6, 2), 8)
  const maxCell = Math.max(...cells.map((cell) => Number(cell.quantity)), 0)

  function slot(cell: CellSummary): [number, number] {
    const entry = Number(cell.entry_price)
    const [from, to] = direction === 'long' ? [entry - spacing, entry] : [entry, entry + spacing]
    return [Math.max(from, low), Math.min(to, high)]
  }

  function track(event: React.MouseEvent<SVGSVGElement>) {
    const box = event.currentTarget.getBoundingClientRect()
    if (box.width <= 0 || box.height <= 0) return
    const px = ((event.clientX - box.left) / box.width) * WIDTH
    const py = ((event.clientY - box.top) / box.height) * height
    const inCore = coreLane !== null && (gridLane === null || py < coreLane.bottom + 4)
    let best: Pick | null = null
    let distance = Infinity
    if (inCore) {
      batches.forEach((batch) => {
        const gap = Math.abs(x(Number(batch.price)) - px)
        if (gap < distance) [best, distance] = [{ kind: 'batch', index: batch.index }, gap]
      })
    } else if (!dense && Math.abs(py - exitY) <= 6) {
      // 停在平仓价那一行：按空心点挑。第 k 格的点落在相邻那格的价格段里，按段挑会挑错。
      cells.forEach((cell) => {
        const gap = Math.abs(x(Number(cell.exit_price)) - px)
        if (gap < distance) [best, distance] = [{ kind: 'cell', index: cell.index }, gap]
      })
    } else {
      cells.forEach((cell) => {
        const [from, to] = slot(cell)
        const gap = px >= x(from) && px <= x(to) ? 0 : Math.min(Math.abs(x(from) - px), Math.abs(x(to) - px))
        if (gap < distance) [best, distance] = [{ kind: 'cell', index: cell.index }, gap]
      })
    }
    setPick(distance <= 12 ? best : null)
  }

  const picked =
    pick === null
      ? null
      : pick.kind === 'batch'
        ? batches.find((batch) => batch.index === pick.index)
        : cells.find((cell) => cell.index === pick.index)

  return (
    <div className="flex flex-col gap-1.5">
      <svg
        viewBox={`0 0 ${WIDTH} ${height}`}
        className="w-full"
        role="img"
        aria-label={`${DIRECTION_LABELS[direction]}侧挂单排布`}
        onMouseMove={track}
        onMouseLeave={() => setPick(null)}
      >
        {coreLane === null ? null : (
          <>
            <text x={0} y={coreLane.label} style={{ fill: 'var(--muted-foreground)', fontSize: 9.5 }}>
              主仓{buy}单 · {batches.length} 批
            </text>
            <line x1={0} x2={WIDTH} y1={coreLane.bottom} y2={coreLane.bottom} stroke="var(--border)" />
            {batches.map((batch) => {
              const at = x(Number(batch.price))
              const share = maxCore > 0 ? Number(batch.core_quantity) / maxCore : 1
              const barHeight = (coreLane.bottom - coreLane.top) * (0.35 + 0.65 * share)
              const lit = started.has(`b${batch.index}`)
              return (
                <rect
                  key={batch.index}
                  data-order={`batch-${batch.index}`}
                  data-started={lit}
                  x={at - barWidth / 2}
                  y={coreLane.bottom - barHeight}
                  width={barWidth}
                  height={barHeight}
                  rx={1.5}
                  style={{
                    fill: solid(direction, lit ? 60 : 32),
                    stroke: tint(direction, lit ? 85 : 35),
                    strokeWidth: pick?.kind === 'batch' && pick.index === batch.index ? 1.6 : 0.8,
                  }}
                />
              )
            })}
          </>
        )}
        {gridLane === null ? null : (
          <>
            <text x={0} y={gridLane.label} style={{ fill: 'var(--muted-foreground)', fontSize: 9.5 }}>
              网格 · {dense ? `${cells.length} 格` : `每格一${buy}一${sell}，空心点是平仓价`}
            </text>
            <line x1={0} x2={WIDTH} y1={gridLane.bottom} y2={gridLane.bottom} stroke="var(--border)" />
            {cells.map((cell) => {
              const [from, to] = slot(cell)
              const inset = dense ? 0 : 0.6
              const left = x(from) + inset
              const width = Math.max(x(to) - x(from) - inset * 2, 0.6)
              const room = gridLane.bottom - gridLane.top - (dense ? 0 : 12)
              const share = maxCell > 0 ? Number(cell.quantity) / maxCell : 1
              const tube = room * (0.45 + 0.55 * share)
              const tubeTop = gridLane.bottom - tube
              const lit = started.has(`c${cell.index}`)
              const chosen = pick?.kind === 'cell' && pick.index === cell.index
              const entry = x(Number(cell.entry_price))
              const exit = x(Number(cell.exit_price))
              return (
                <g key={cell.index}>
                  <rect
                    data-order={`cell-${cell.index}`}
                    data-started={lit}
                    x={left}
                    y={tubeTop}
                    width={width}
                    height={tube}
                    rx={dense ? 0 : 1.5}
                    style={{
                      fill: tint(direction, lit ? 22 : 5),
                      stroke: chosen ? 'var(--foreground)' : tint(direction, lit ? 90 : 35),
                      strokeWidth: chosen ? 1.2 : 0.8,
                    }}
                  />
                  {dense ? null : (
                    <>
                      <path
                        d={`M${entry} ${tubeTop} V${exitY} H${exit}`}
                        style={{ fill: 'none', stroke: tint(direction, chosen ? 90 : 30), strokeWidth: 0.8 }}
                      />
                      <circle
                        cx={exit}
                        cy={exitY}
                        r={1.8}
                        style={{ fill: 'var(--background)', stroke: tint(direction, lit || chosen ? 90 : 45) }}
                      />
                    </>
                  )}
                </g>
              )
            })}
          </>
        )}
        <AxisMarks axis={axis} top={top} bottom={bottom} />
      </svg>
      <p className="num text-[11px] text-muted-foreground" aria-live="polite">
        {picked === undefined || picked === null || pick === null ? (
          dense ? (
            '格子太密，图上不画平仓价；移到某一格上看它的价格和数量。'
          ) : (
            '移到某一张单上看它的价格、数量和每轮利润。'
          )
        ) : pick.kind === 'batch' && 'core_quantity' in picked ? (
          <>
            主仓批 {picked.index} · {buy} <span className="text-foreground">{picked.price}</span> ·{' '}
            <Private>{Number(picked.core_quantity).toFixed(quantityPlaces)}</Private> · 约{' '}
            <Private>{(Number(picked.price) * Number(picked.core_quantity)).toFixed(2)}</Private> USDC
            {started.has(`b${picked.index}`) ? ' · 启动即成交' : ''}
          </>
        ) : 'entry_price' in picked ? (
          <>
            格 {picked.index}
            {picked.batch === null ? '' : `（批 ${picked.batch}）`} · {buy}{' '}
            <span className="text-foreground">{picked.entry_price}</span> → {sell}{' '}
            <span className="text-foreground">{picked.exit_price}</span> ·{' '}
            <Private>{Number(picked.quantity).toFixed(quantityPlaces)}</Private> · 每轮{' '}
            <span className="text-long">
              +<Private>{Number(picked.profit_maker).toFixed(2)}</Private>
            </span>
            {started.has(`c${picked.index}`) ? ' · 启动即成交' : ''}
          </>
        ) : null}
      </p>
    </div>
  )
}
