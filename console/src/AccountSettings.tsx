// 账户风控设置（设计第 7.4、7.6、11 节）：保证金率的提醒阈值、停止阈值、行情过期时间和
// 资金费率的提醒阈值。
//
// 保证金是整个账户共用的，这几项按账户设，所有策略共用一份。校验在服务端：页面只把
// 百分比换成比率交上去，不合理的设置由服务端说明原因。

import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { Callout, PageHeader } from '@/components/layout'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { api } from './api'
import { percentInputToRatio, ratioToPercentInput } from './format'

interface AccountSettingsProps {
  onBack: () => void
}

export function AccountSettingsView({ onBack }: AccountSettingsProps) {
  const queryClient = useQueryClient()
  const settings = useQuery({ queryKey: ['account-settings'], queryFn: api.accountSettings })
  const [alert, setAlert] = useState('')
  const [stop, setStop] = useState('')
  const [maxAge, setMaxAge] = useState('')
  const [funding, setFunding] = useState('')
  const [problem, setProblem] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    if (settings.data === undefined) return
    setAlert(ratioToPercentInput(settings.data.margin_alert_ratio))
    setStop(ratioToPercentInput(settings.data.margin_stop_ratio))
    setMaxAge(String(settings.data.market_data_max_age))
    setFunding(ratioToPercentInput(settings.data.funding_alert_annual_rate))
  }, [settings.data])

  const save = useMutation({
    mutationFn: api.updateAccountSettings,
    onSuccess: (next) => {
      queryClient.setQueryData(['account-settings'], next)
      setSaved(true)
    },
    onError: (error: Error) => setProblem(error.message),
  })

  // 改了任何一项，「已保存」就不再成立：页面上的数和服务端在用的那份不一样了。保存期间
  // 输入框禁用：不然响应回来时，页面上的数已经不是交上去的那份，却会显示「已保存」。
  const edit = (set: (value: string) => void, value: string) => {
    set(value)
    setSaved(false)
  }

  const submit = () => {
    setSaved(false)
    const alertRatio = percentInputToRatio(alert)
    const stopRatio = percentInputToRatio(stop)
    const fundingRatio = percentInputToRatio(funding)
    if (alertRatio === null || stopRatio === null || fundingRatio === null) {
      setProblem('阈值填百分比数字。')
      return
    }
    // 秒数先按整数字符串认，再确认转换没有丢精度：不能把用户填的数悄悄改成别的。
    const seconds = Number(maxAge.trim())
    if (!/^\d+$/.test(maxAge.trim()) || !Number.isSafeInteger(seconds) || seconds < 1) {
      setProblem('行情过期时间填正整数（秒）。')
      return
    }
    setProblem(null)
    save.mutate({
      margin_alert_ratio: alertRatio,
      margin_stop_ratio: stopRatio,
      market_data_max_age: seconds,
      funding_alert_annual_rate: fundingRatio,
    })
  }

  return (
    <div className="mx-auto flex w-full max-w-3xl flex-col gap-6">
      <PageHeader
        leading={
          <button
            type="button"
            onClick={onBack}
            className="w-fit text-xs text-muted-foreground transition-colors hover:text-foreground"
          >
            ← 返回列表
          </button>
        }
        title="账户风控设置"
        subtitle="全账户共用"
      />
      {settings.error ? <Callout tone="danger" title={`读不到设置：${settings.error.message}`} /> : null}

      <Card
        title="保证金率"
        description="超过停止阈值：全账户只留只减仓单，回落后恢复。"
      >
        <MarginScale alert={alert} stop={stop} />
        <div className="grid grid-cols-2 gap-4">
          <Field
            label="提醒阈值（%）"
            value={alert}
            disabled={save.isPending}
            onChange={(value) => edit(setAlert, value)}
          />
          <Field
            label="停止阈值（%）"
            value={stop}
            disabled={save.isPending}
            onChange={(value) => edit(setStop, value)}
          />
        </div>
      </Card>

      <Card title="行情与资金费">
        <div className="grid grid-cols-2 gap-4">
          <Field
            label="行情过期时间（秒）"
            hint="更旧的读数视为读不到"
            value={maxAge}
            disabled={save.isPending}
            onChange={(value) => edit(setMaxAge, value)}
          />
          <Field
            label="资金费率提醒阈值（年化 %）"
            hint="仅提醒，不自动交易"
            value={funding}
            disabled={save.isPending}
            onChange={(value) => edit(setFunding, value)}
          />
        </div>
      </Card>

      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={submit} disabled={save.isPending}>
          保存
        </Button>
        {saved ? <span className="text-xs text-long">已保存，下一步起生效。</span> : null}
      </div>
      {problem ? <Callout tone="danger" title={problem} /> : null}
    </div>
  )
}

function Card({
  title,
  description,
  children,
}: {
  title: string
  description?: string
  children: React.ReactNode
}) {
  return (
    <section aria-label={title} className="flex flex-col gap-4 rounded-lg bg-card p-4 ring-1 ring-border">
      <div className="flex flex-col gap-1">
        <h2 className="font-heading text-sm font-semibold tracking-tight">{title}</h2>
        {description === undefined ? null : (
          <p className="text-xs/relaxed text-muted-foreground">{description}</p>
        )}
      </div>
      {children}
    </section>
  )
}

function Field({
  label,
  hint,
  value,
  disabled,
  onChange,
}: {
  label: string
  hint?: string
  value: string
  disabled: boolean
  onChange: (value: string) => void
}) {
  return (
    <label className="flex min-w-0 flex-col gap-1">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <Input
        aria-label={label}
        className="num h-8"
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      />
      {hint === undefined ? null : (
        <span className="text-[10px] leading-snug text-muted-foreground/80">{hint}</span>
      )}
    </label>
  )
}

/**
 * 0–100% 的保证金率刻度：安全、提醒、停止三段，跟着输入框变。到 100% 维持保证金占满，
 * 账户会被强平。只画用户填的数；填得不是 0–100 之间、或者停止不高于提醒时不画，
 * 原因由保存时的校验说明。
 */
function MarginScale({ alert, stop }: { alert: string; stop: string }) {
  const a = Number(alert)
  const s = Number(stop)
  if (alert.trim() === '' || stop.trim() === '' || !(a > 0 && a < s && s < 100)) return null
  return (
    <div aria-hidden className="flex flex-col gap-1">
      <div className="flex h-2 overflow-hidden rounded-full">
        <div className="bg-long/45" style={{ width: `${a}%` }} />
        <div className="bg-warn/55" style={{ width: `${s - a}%` }} />
        <div className="bg-destructive/60" style={{ width: `${100 - s}%` }} />
      </div>
      <div className="num relative h-4 text-[10px] text-muted-foreground">
        <span className="absolute left-0">0%</span>
        <span className="absolute -translate-x-1/2 text-warn" style={{ left: `${a}%` }}>
          提醒 {alert}%
        </span>
        <span className="absolute -translate-x-1/2 text-destructive" style={{ left: `${s}%` }}>
          停止 {stop}%
        </span>
        <span className="absolute right-0">100% 强平</span>
      </div>
    </div>
  )
}
