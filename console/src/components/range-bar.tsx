// 价格条：一条横轴上画出多头侧、空头侧的建仓范围、观点失效价，以及最近的标记价。
//
// 只用来定位：价格换成横轴上的位置，不参与任何交易数量的计算。

import { cn } from '@/lib/utils'
import type { SideConfig, StrategyConfig } from '@/types'

interface Segment {
  low: number
  high: number
  tone: 'long' | 'short'
}

function finite(value: string | null | undefined): number | null {
  if (value === null || value === undefined || value === '') return null
  const parsed = Number(value)
  return Number.isFinite(parsed) ? parsed : null
}

function segment(side: SideConfig | null, tone: 'long' | 'short'): Segment | null {
  if (side === null) return null
  const low = finite(side.price_low)
  const high = finite(side.price_high)
  return low === null || high === null ? null : { low, high, tone }
}

interface RangeBarProps {
  config: StrategyConfig
  /** 标记价；没有读数时不画。 */
  mark?: string | null
  className?: string
  /** 两端标出价格范围。 */
  labels?: boolean
}

export function RangeBar({ config, mark, className, labels = true }: RangeBarProps) {
  const segments = [segment(config.long, 'long'), segment(config.short, 'short')].filter(
    (value): value is Segment => value !== null,
  )
  const invalidations = [config.long?.invalidation?.price, config.short?.invalidation?.price]
    .map(finite)
    .filter((value): value is number => value !== null)
  const markValue = finite(mark)
  const points = [
    ...segments.flatMap((part) => [part.low, part.high]),
    ...invalidations,
    ...(markValue === null || markValue <= 0 ? [] : [markValue]),
  ]
  if (segments.length === 0 || points.length === 0) return null
  const min = Math.min(...points)
  const max = Math.max(...points)
  const pad = (max - min) * 0.06 || max * 0.01 || 1
  const lo = min - pad
  const span = max + pad - lo
  const fraction = (value: number) => (value - lo) / span
  const at = (value: number) => `${fraction(value) * 100}%`
  const low = Math.min(...segments.map((part) => part.low))
  const high = Math.max(...segments.map((part) => part.high))
  // 标记价的标签离端点太近时让端点让开：标记价是这一刻最要紧的数。
  const crowded = (value: number) =>
    markValue !== null && markValue > 0 && Math.abs(fraction(value) - fraction(markValue)) < 0.14

  return (
    <div className={cn('w-full', className)}>
      <div className="relative h-7">
        {/* 轴线 */}
        <div className="absolute inset-x-0 top-1/2 h-px -translate-y-1/2 bg-border" />
        {segments.map((part) => (
          <div
            key={part.tone}
            className={cn(
              'absolute top-1/2 h-2.5 -translate-y-1/2 rounded-[3px] ring-1 ring-inset',
              part.tone === 'long' ? 'bg-long/25 ring-long/50' : 'bg-short/25 ring-short/50',
            )}
            style={{ left: at(part.low), width: `calc(${at(part.high)} - ${at(part.low)})` }}
          />
        ))}
        {invalidations.map((value) => (
          <div
            key={`inv-${value}`}
            title={`观点失效价 ${value}`}
            className="absolute top-1 bottom-1 w-px border-l border-dashed border-destructive"
            style={{ left: at(value) }}
          />
        ))}
        {markValue === null || markValue <= 0 ? null : (
          <div
            className="absolute top-0 bottom-0 flex -translate-x-1/2 flex-col items-center"
            style={{ left: at(markValue) }}
            title={`标记价 ${mark}`}
          >
            <div className="h-full w-0.5 rounded-full bg-signal shadow-[0_0_8px] shadow-signal/60" />
          </div>
        )}
      </div>
      {labels ? (
        <div className="relative mt-0.5 h-4 text-[10px] text-muted-foreground num">
          {crowded(low) ? null : (
            <span className="absolute -translate-x-1/2" style={{ left: at(low) }}>
              {low}
            </span>
          )}
          {crowded(high) ? null : (
            <span className="absolute -translate-x-1/2" style={{ left: at(high) }}>
              {high}
            </span>
          )}
          {markValue === null || markValue <= 0 ? null : (
            <span
              className="absolute -translate-x-1/2 font-semibold text-signal"
              style={{ left: at(markValue) }}
            >
              {mark}
            </span>
          )}
        </div>
      ) : null}
    </div>
  )
}
