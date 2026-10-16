// 账户风控设置（设计第 7.4、11 节）：保证金率的提醒阈值、停止阈值和行情过期时间。
//
// 保证金是整个账户共用的，这几项按账户设，所有策略共用一份。校验在服务端：页面只把
// 百分比换成比率交上去，不合理的设置由服务端说明原因。

import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

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
  const [problem, setProblem] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  useEffect(() => {
    if (settings.data === undefined) return
    setAlert(ratioToPercentInput(settings.data.margin_alert_ratio))
    setStop(ratioToPercentInput(settings.data.margin_stop_ratio))
    setMaxAge(String(settings.data.market_data_max_age))
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
    if (alertRatio === null || stopRatio === null) {
      setProblem('阈值要填百分比数字，例如 30。')
      return
    }
    // 秒数先按整数字符串认，再确认转换没有丢精度：不能把用户填的数悄悄改成别的。
    const seconds = Number(maxAge.trim())
    if (!/^\d+$/.test(maxAge.trim()) || !Number.isSafeInteger(seconds) || seconds < 1) {
      setProblem('行情过期时间要填正整数秒。')
      return
    }
    setProblem(null)
    save.mutate({
      margin_alert_ratio: alertRatio,
      margin_stop_ratio: stopRatio,
      market_data_max_age: seconds,
    })
  }

  return (
    <section className="settings">
      <div className="actions">
        <button type="button" onClick={onBack}>
          返回列表
        </button>
      </div>
      <h2>账户风控设置</h2>
      <p className="hint">
        保证金率超过提醒阈值时提醒；超过停止阈值时，同一账户所有策略只发、只保留只减仓单，回落后自动恢复。
      </p>
      {settings.error ? <p className="error">读不到设置：{settings.error.message}</p> : null}
      <label>
        提醒阈值（%）
        <input
          value={alert}
          disabled={save.isPending}
          onChange={(event) => edit(setAlert, event.target.value)}
        />
      </label>
      <label>
        停止阈值（%）
        <input
          value={stop}
          disabled={save.isPending}
          onChange={(event) => edit(setStop, event.target.value)}
        />
      </label>
      <label>
        行情过期时间（秒）
        <input
          value={maxAge}
          disabled={save.isPending}
          onChange={(event) => edit(setMaxAge, event.target.value)}
        />
      </label>
      <div className="actions">
        <button type="button" className="primary" onClick={submit} disabled={save.isPending}>
          保存
        </button>
      </div>
      {problem ? <p className="error">{problem}</p> : null}
      {saved ? <p className="ok">已保存，下一步起按新设置判断。</p> : null}
    </section>
  )
}
