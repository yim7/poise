// 状态、类型、网络三种标识。颜色只表达意思：一眼能分清在跑、在收尾、停了、出了问题。

import { cn } from '@/lib/utils'
import { kindLabel, statusLabel } from '@/labels'
import type { InstanceInfo, StrategyConfig, StrategyStatus } from '@/types'

type Tone = 'long' | 'short' | 'warn' | 'danger' | 'muted' | 'info' | 'signal'

const TONE: Record<Tone, string> = {
  long: 'text-long bg-long/12 ring-long/25',
  short: 'text-short bg-short/12 ring-short/25',
  warn: 'text-warn bg-warn/12 ring-warn/25',
  danger: 'text-destructive bg-destructive/14 ring-destructive/30',
  muted: 'text-muted-foreground bg-muted/60 ring-border',
  info: 'text-info bg-info/12 ring-info/25',
  signal: 'text-signal bg-signal/12 ring-signal/25',
}

const DOT: Record<Tone, string> = {
  long: 'bg-long',
  short: 'bg-short',
  warn: 'bg-warn',
  danger: 'bg-destructive',
  muted: 'bg-muted-foreground',
  info: 'bg-info',
  signal: 'bg-signal',
}

function Pill({
  tone,
  dot,
  pulse,
  className,
  children,
  title,
}: {
  tone: Tone
  dot?: boolean
  pulse?: boolean
  className?: string
  children: React.ReactNode
  title?: string
}) {
  return (
    <span
      title={title}
      className={cn(
        'inline-flex h-5 items-center gap-1.5 rounded-full px-2 text-[11px] font-medium whitespace-nowrap ring-1 ring-inset',
        TONE[tone],
        className,
      )}
    >
      {dot ? (
        <span className="relative flex size-1.5">
          {pulse ? (
            <span className={cn('absolute inline-flex size-full animate-ping rounded-full opacity-60', DOT[tone])} />
          ) : null}
          <span className={cn('relative inline-flex size-1.5 rounded-full', DOT[tone])} />
        </span>
      ) : null}
      {children}
    </span>
  )
}

/** 状态的颜色：在跑是绿、在收尾是琥珀（会动）、失效是红、停了是灰。 */
const STATUS_TONE: Record<StrategyStatus, { tone: Tone; pulse: boolean }> = {
  draft: { tone: 'info', pulse: false },
  running: { tone: 'long', pulse: true },
  restoring: { tone: 'warn', pulse: true },
  pausing: { tone: 'warn', pulse: true },
  paused: { tone: 'muted', pulse: false },
  stopping: { tone: 'warn', pulse: true },
  stopped: { tone: 'muted', pulse: false },
  invalidating: { tone: 'danger', pulse: true },
  invalidated: { tone: 'danger', pulse: false },
  closing: { tone: 'warn', pulse: true },
  closed: { tone: 'muted', pulse: false },
}

export function StatusBadge({ status }: { status: StrategyStatus }) {
  const { tone, pulse } = STATUS_TONE[status] ?? { tone: 'muted', pulse: false }
  return (
    <Pill tone={tone} dot pulse={pulse}>
      {statusLabel(status)}
    </Pill>
  )
}

/** 做多、做空、中性：和价格条上两侧的颜色一致。 */
export function KindBadge({ config }: { config: StrategyConfig }) {
  const label = kindLabel(config)
  const tone: Tone = label === '做多' ? 'long' : label === '做空' ? 'short' : 'info'
  return <Pill tone={tone}>{label}</Pill>
}

/** 账户地址只显示头尾：核对用，完整地址放在悬停提示里。 */
export function shortAddress(address: string): string {
  return address.length > 12 ? `${address.slice(0, 6)}…${address.slice(-4)}` : address
}

/**
 * 网络标识：正式网是实盘，用最显眼的琥珀实底；测试网用蓝色描边。认错网络就是在实盘上操作，
 * 所以这个标识在每一屏的顶栏都看得见。
 */
export function NetworkBadge({ instance }: { instance: InstanceInfo | undefined }) {
  if (instance === undefined) return null
  if (instance.network === 'mainnet') {
    return (
      <span className="inline-flex h-6 items-center gap-1.5 rounded-md bg-warn px-2.5 text-[11px] font-semibold text-black">
        <span className="size-1.5 rounded-full bg-black/70" />
        主网 · 实盘
      </span>
    )
  }
  if (instance.network === 'testnet') {
    return (
      <Pill tone="info" dot className="h-6 rounded-md">
        测试网
      </Pill>
    )
  }
  return (
    <Pill tone="muted" className="h-6 rounded-md">
      网络未绑定
    </Pill>
  )
}

export { Pill }
export type { Tone }
