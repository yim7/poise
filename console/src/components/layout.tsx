// 页面骨架：每屏的标题行、区块标题、出错和提醒的横条。

import { cn } from '@/lib/utils'

export function PageHeader({
  title,
  subtitle,
  actions,
  leading,
}: {
  title: React.ReactNode
  subtitle?: React.ReactNode
  actions?: React.ReactNode
  leading?: React.ReactNode
}) {
  return (
    <div className="flex flex-wrap items-end justify-between gap-3">
      <div className="flex min-w-0 flex-col gap-1">
        {leading}
        <h1 className="font-heading text-2xl font-semibold tracking-tight">{title}</h1>
        {subtitle === undefined ? null : (
          <div className="text-xs text-muted-foreground">{subtitle}</div>
        )}
      </div>
      {actions === undefined ? null : <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  )
}

export function SectionTitle({
  children,
  count,
  action,
}: {
  children: React.ReactNode
  count?: number
  action?: React.ReactNode
}) {
  return (
    <div className="mb-3 flex items-center justify-between gap-3">
      <h2 className="flex items-baseline gap-2 font-heading text-sm font-semibold tracking-tight">
        {children}
        {count === undefined ? null : (
          <span className="num text-xs font-normal text-muted-foreground">{count}</span>
        )}
      </h2>
      {action}
    </div>
  )
}

type CalloutTone = 'info' | 'warn' | 'danger' | 'ok'

const CALLOUT: Record<CalloutTone, string> = {
  info: 'border-info/30 bg-info/8 text-info',
  warn: 'border-warn/35 bg-warn/8 text-warn',
  danger: 'border-destructive/40 bg-destructive/10 text-destructive',
  ok: 'border-long/30 bg-long/8 text-long',
}

/** 一条醒目的说明：出错、提醒、等待原因。正文用前景色，左边的色条和标题表达轻重。 */
export function Callout({
  tone,
  title,
  children,
  className,
}: {
  tone: CalloutTone
  title?: React.ReactNode
  children?: React.ReactNode
  className?: string
}) {
  return (
    <div
      role={tone === 'danger' ? 'alert' : undefined}
      className={cn('rounded-md border border-l-[3px] px-3 py-2 text-xs/relaxed', CALLOUT[tone], className)}
    >
      {title === undefined ? null : <div className="font-medium">{title}</div>}
      {children === undefined || children === null ? null : (
        <div className="text-foreground/85">{children}</div>
      )}
    </div>
  )
}
