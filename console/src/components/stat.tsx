// 一项读数：小标签在上，等宽大数字在下，可选一行补充说明。

import { cn } from '@/lib/utils'

export type StatTone = 'default' | 'long' | 'short' | 'warn' | 'danger' | 'signal' | 'muted'

const TONE: Record<StatTone, string> = {
  default: 'text-foreground',
  long: 'text-long',
  short: 'text-short',
  warn: 'text-warn',
  danger: 'text-destructive',
  signal: 'text-signal',
  muted: 'text-muted-foreground',
}

interface StatProps {
  label: string
  value: React.ReactNode
  hint?: React.ReactNode
  tone?: StatTone
  size?: 'lg' | 'md'
  className?: string
}

export function Stat({ label, value, hint, tone = 'default', size = 'md', className }: StatProps) {
  return (
    <div className={cn('flex min-w-0 flex-col gap-1', className)}>
      <span className="eyebrow">{label}</span>
      <span
        className={cn(
          // 读数不截断：藏掉的位数正是要看的数。放不下就换行，窄屏先缩小一号。
          'num leading-tight font-medium break-all',
          size === 'lg' ? 'text-xl sm:text-2xl' : 'text-base',
          TONE[tone],
        )}
      >
        {value}
      </span>
      {hint === undefined || hint === null ? null : (
        <span className="truncate text-[11px] text-muted-foreground">{hint}</span>
      )}
    </div>
  )
}

/** 仓位按方向着色：多为绿、空为红、零为灰。 */
export function positionTone(value: string | null | undefined): StatTone {
  const parsed = Number(value)
  if (!Number.isFinite(parsed) || parsed === 0) return 'muted'
  return parsed > 0 ? 'long' : 'short'
}
