// 价格条：一条横轴上画出每一侧的格子（持有多少填多少）、观点失效价、标记价，详情页再标出
// 持仓均价和保本价。
//
// 只用来定位：价格换成横轴上的位置，不参与任何交易数量的计算。
//
// 格子不画边框，靠 2px 的空隙分开；空格子很淡，持有的部分半透明填色，部分持有只填左边
// 那一截。格子太密（一侧超过 60 格）时空隙会把格子吃掉，就不留空隙，连成一条。

import { type RefObject, useEffect, useRef, useState } from 'react'

import { cn } from '@/lib/utils'
import type { CellSummary, Direction, SideConfig, StrategyConfig } from '@/types'

/** 一侧的价格范围。 */
interface Segment {
  low: number
  high: number
  tone: Direction
}

/** 一格在横轴上占的那段价格，和持有比例。 */
interface Slot {
  key: string
  low: number
  high: number
  held: number
  tone: Direction
}

/** 一侧格子超过这么多就不留空隙。 */
const DENSE = 60

function finite(value: string | null | undefined): number | null {
  if (value === null || value === undefined || value === '') return null
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : null
}

function segment(side: SideConfig | null, tone: Direction): Segment | null {
  if (side === null) return null
  const low = finite(side.price_low)
  const high = finite(side.price_high)
  return low === null || high === null ? null : { low, high, tone }
}

/**
 * 每一格在横轴上的那段：多头侧的格子从 H 往下排，建仓价是这一段的上沿；空头侧从 L 往上排，
 * 建仓价是下沿。段长是网格间距，截在这一侧的范围里。
 */
function slots(config: StrategyConfig, cells: CellSummary[]): Slot[] {
  return cells.flatMap((cell) => {
    const side = cell.direction === 'long' ? config.long : config.short
    const range = segment(side, cell.direction)
    const spacing = finite(side?.grid_spacing)
    const entry = finite(cell.entry_price)
    const quantity = finite(cell.quantity)
    const held = finite(cell.held)
    if (range === null || spacing === null || entry === null) return []
    const [low, high] = cell.direction === 'long' ? [entry - spacing, entry] : [entry, entry + spacing]
    const ratio = quantity === null || quantity <= 0 || held === null ? 0 : Math.min(Math.max(held / quantity, 0), 1)
    return [
      {
        key: `${cell.direction}-${cell.index}`,
        low: Math.max(low, range.low),
        high: Math.min(high, range.high),
        held: ratio,
        tone: cell.direction,
      },
    ]
  })
}

interface RangeBarProps {
  config: StrategyConfig
  /** 标记价；没有读数时不画。 */
  mark?: string | null
  /** 格子表；没有时只画每一侧的范围。 */
  cells?: CellSummary[]
  /** 持仓均价和保本价，只在详情页给。 */
  average?: string | null
  breakeven?: string | null
  /** `full` 标出失效价、均价、保本价的数字；`compact` 只标范围两端和标记价。 */
  variant?: 'full' | 'compact'
  className?: string
}

/** 一个文字标注：价格、文字、颜色，和避让时的先后（数字小的先占位置）。 */
interface Label {
  value: number
  text: string
  className: string
  rank: number
  /** 文字对着刻度的哪一边：`end` 在刻度左边，`start` 在右边。 */
  align: 'center' | 'start' | 'end'
}

/** 估一个标注有多宽（像素）：10px 的字，汉字约 10px、数字和字母约 6.2px，加上内边距。 */
function textWidth(text: string): number {
  return [...text].reduce((total, char) => total + (/[\u4e00-\u9fff]/.test(char) ? 10 : 6.2), 4)
}

/** 一个标注在横轴上占的那段（占整条的比例）。`width` 是整条的像素宽度。 */
function extent(label: Label, fraction: (value: number) => number, width: number): [number, number] {
  const size = textWidth(label.text) / width
  const at = fraction(label.value)
  if (label.align === 'start') return [at, at + size]
  if (label.align === 'end') return [at - size, at]
  return [at - size / 2, at + size / 2]
}

/** 按先后放标注：后来的和已经放下的挨得太近就不放。 */
function place(labels: Label[], fraction: (value: number) => number, width: number): Label[] {
  const placed: Label[] = []
  for (const label of [...labels].sort((a, b) => a.rank - b.rank)) {
    const [from, to] = extent(label, fraction, width)
    const free = placed.every((other) => {
      const [otherFrom, otherTo] = extent(other, fraction, width)
      return to + 0.01 <= otherFrom || otherTo + 0.01 <= from
    })
    if (free) placed.push(label)
  }
  return placed
}

/** 量出元素的宽度，跟着窗口变；量不到时（测试环境）按 800px 算。 */
function useWidth(): [RefObject<HTMLDivElement>, number] {
  const ref = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(800)
  useEffect(() => {
    const node = ref.current
    if (node === null || typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(([entry]) => {
      if (entry.contentRect.width > 0) setWidth(entry.contentRect.width)
    })
    observer.observe(node)
    return () => observer.disconnect()
  }, [])
  return [ref, width]
}

export function RangeBar({
  config,
  mark,
  cells = [],
  average,
  breakeven,
  variant = 'compact',
  className,
}: RangeBarProps) {
  const [ref, width] = useWidth()
  const segments = [segment(config.long, 'long'), segment(config.short, 'short')].filter(
    (value): value is Segment => value !== null,
  )
  const invalidations = (
    [
      ['long', config.long?.invalidation?.price],
      ['short', config.short?.invalidation?.price],
    ] as const
  ).flatMap(([tone, price]) => {
    const value = finite(price)
    return value === null ? [] : [{ tone, value }]
  })
  const markValue = finite(mark)
  const hasMark = markValue !== null && markValue > 0
  const averageValue = finite(average)
  const breakevenValue = finite(breakeven)
  const points = [
    ...segments.flatMap((part) => [part.low, part.high]),
    ...invalidations.map((line) => line.value),
    ...(hasMark ? [markValue] : []),
    ...(averageValue === null ? [] : [averageValue]),
    ...(breakevenValue === null ? [] : [breakevenValue]),
  ]
  if (segments.length === 0 || points.length === 0) return null
  const min = Math.min(...points)
  const max = Math.max(...points)
  const pad = (max - min) * 0.06 || max * 0.01 || 1
  const lo = min - pad
  const span = max + pad - lo
  const fraction = (value: number) => (value - lo) / span
  const at = (value: number) => `${fraction(value) * 100}%`
  const full = variant === 'full'

  const cellSlots = slots(config, cells)
  const dense = (tone: Direction) => cellSlots.filter((slot) => slot.tone === tone).length > DENSE
  // 没有格子表时（例如参数还没校验通过），每一侧画成一整格空的。
  const drawn: Slot[] =
    cellSlots.length > 0
      ? cellSlots
      : segments.map((part) => ({ key: part.tone, low: part.low, high: part.high, held: 0, tone: part.tone }))

  const low = Math.min(...segments.map((part) => part.low))
  const high = Math.max(...segments.map((part) => part.high))
  // 下面一行的标注：放不下时按先后保留，后来的离已经占住的位置太近就不画。
  // 均价和保本价常常挨得很近：低的那个文字放在刻度左边，高的放右边，各自往外让。
  const breakevenLower =
    breakevenValue !== null && averageValue !== null ? breakevenValue <= averageValue : true
  const below: Label[] = [
    ...(full && breakevenValue !== null
      ? [
          {
            value: breakevenValue,
            text: `保本 ${breakeven}`,
            className: 'text-warn',
            rank: 0,
            align: breakevenLower ? ('end' as const) : ('start' as const),
          },
        ]
      : []),
    ...(full && averageValue !== null
      ? [
          {
            value: averageValue,
            text: `均价 ${average}`,
            className: 'text-foreground/80',
            rank: 1,
            align: breakevenLower ? ('start' as const) : ('end' as const),
          },
        ]
      : []),
    ...(!full && hasMark
      ? [{ value: markValue, text: String(mark), className: 'font-semibold text-signal', rank: 0, align: 'center' as const }]
      : []),
    { value: low, text: String(low), className: 'text-muted-foreground', rank: 2, align: 'center' },
    { value: high, text: String(high), className: 'text-muted-foreground', rank: 2, align: 'center' },
  ]
  const placed = place(below, fraction, width)
  // 上面一行：标记价先占位置，失效价的文字挨得太近就不写（红线和悬停提示还在）。
  const above = place(
    [
      ...(hasMark
        ? [{ value: markValue, text: String(mark), className: 'text-[11px] font-semibold text-signal', rank: 0, align: 'center' as const }]
        : []),
      ...invalidations.map((line) => ({
        value: line.value,
        text: `失效 ${line.value}`,
        className: 'text-destructive/90',
        rank: 1,
        align: line.tone === 'long' ? ('start' as const) : ('end' as const),
      })),
    ],
    fraction,
    width,
  )

  return (
    <div ref={ref} className={cn('w-full', className)}>
      {full ? <LabelRow labels={above} at={at} /> : null}
      <div className="relative h-5">
        {/* 轴线 */}
        <div className="absolute inset-x-0 top-1/2 h-px -translate-y-1/2 bg-foreground/10" />
        {/* 失效价以外：多头侧在失效价下方，空头侧在上方 */}
        {invalidations.map((line) => (
          <div
            key={`inv-zone-${line.tone}`}
            className="absolute top-1/2 h-2 -translate-y-1/2 bg-destructive/10"
            style={
              line.tone === 'long'
                ? { left: 0, width: at(line.value) }
                : { left: at(line.value), right: 0 }
            }
          />
        ))}
        {drawn.map((slot) => {
          const gap = dense(slot.tone) ? '0px' : '1px'
          return (
            <div
              key={slot.key}
              data-held={slot.held}
              className={cn(
                'absolute top-1/2 h-2 -translate-y-1/2 overflow-hidden',
                slot.tone === 'long' ? 'bg-long/[0.07]' : 'bg-short/[0.07]',
              )}
              style={{
                left: `calc(${at(slot.low)} + ${gap})`,
                width: `calc(${at(slot.high)} - ${at(slot.low)} - 2 * ${gap})`,
              }}
            >
              {slot.held > 0 ? (
                <div
                  className={cn('h-full', slot.tone === 'long' ? 'bg-long/35' : 'bg-short/35')}
                  style={{ width: `${slot.held * 100}%` }}
                />
              ) : null}
            </div>
          )
        })}
        {invalidations.map((line) => (
          <div
            key={`inv-${line.tone}`}
            title={`观点失效价 ${line.value}`}
            className="absolute top-0.5 bottom-0.5 w-px bg-destructive/80"
            style={{ left: at(line.value) }}
          />
        ))}
        {full && breakevenValue !== null ? (
          <div
            title={`保本价 ${breakeven}`}
            className="absolute bottom-0 h-1.5 w-px -translate-x-1/2 bg-warn"
            style={{ left: at(breakevenValue) }}
          />
        ) : null}
        {full && averageValue !== null ? (
          <div
            title={`持仓均价 ${average}`}
            className="absolute bottom-0 h-1.5 w-px -translate-x-1/2 bg-foreground/80"
            style={{ left: at(averageValue) }}
          />
        ) : null}
        {hasMark ? (
          <div
            title={`标记价 ${mark}`}
            className="absolute top-0 bottom-0 w-[1.5px] -translate-x-1/2 bg-signal"
            style={{ left: at(markValue) }}
          />
        ) : null}
      </div>
      <LabelRow labels={placed} at={at} className="mt-0.5" />
    </div>
  )
}

function LabelRow({
  labels,
  at,
  className,
}: {
  labels: Label[]
  at: (value: number) => string
  className?: string
}) {
  return (
    <div className={cn('relative h-4 text-[10px] num', className)}>
      {labels.map((label) => (
        <span
          key={label.text}
          className={cn(
            'absolute whitespace-nowrap',
            label.align === 'center' && '-translate-x-1/2',
            label.align === 'end' && '-translate-x-full pr-1',
            label.align === 'start' && 'pl-1',
            label.className,
          )}
          style={{ left: at(label.value) }}
        >
          {label.text}
        </span>
      ))}
    </div>
  )
}
