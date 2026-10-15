// 运行中可以改的参数（设计第 2.4 节）：目前只有限速间隔。
//
// 其他参数改不了：服务端会拒绝，并提示先停止再新建。这里只提交限速这一项。

import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'

import { HttpError, api } from './api'
import { parseThrottle, throttleMinutes } from './format'
import type { StrategyConfig } from './types'

interface ThrottleEditorProps {
  strategyId: string
  config: StrategyConfig
}

/**
 * 保存失败时该怎么跟用户说。
 *
 * 「正忙」是常事：常驻服务每隔几秒走一步，正好撞上就报这个。告诉用户等一下再点，
 * 而不是把服务端那句「已经被驱动」原样端上来。
 */
function describeSaveFailure(failure: unknown): string {
  if (failure instanceof HttpError && failure.kind === 'busy') {
    return '这个策略正在走一步，稍等一下再点保存。'
  }
  return failure instanceof Error ? failure.message : String(failure)
}

export function ThrottleEditor({ strategyId, config }: ThrottleEditorProps) {
  const [minutes, setMinutes] = useState(throttleMinutes(config.long.throttle_interval))
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const client = useQueryClient()

  async function save() {
    // 输入不合法就什么都不提交：把「写错了」当成「不限速」提交上去，用户以为限了速，
    // 实际提交的是另一份参数。
    const throttle = parseThrottle(minutes)
    if ('problem' in throttle) {
      setError(throttle.problem)
      setNotice(null)
      return
    }
    setBusy(true)
    setError(null)
    setNotice(null)
    const next: StrategyConfig = {
      ...config,
      long: { ...config.long, throttle_interval: throttle.interval },
    }
    try {
      const saved = await api.updateConfig(strategyId, next)
      setNotice(`限速间隔已生效，记为第 ${saved.version} 版。`)
      await client.invalidateQueries({ queryKey: ['strategy', strategyId] })
    } catch (failure) {
      setError(describeSaveFailure(failure))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="throttle">
      <h3>运行中可改的参数</h3>
      <label className="field">
        <span>限速间隔 τ（分钟）</span>
        <input
          aria-label="限速间隔 τ（分钟）"
          value={minutes}
          disabled={busy}
          onChange={(event) => setMinutes(event.target.value)}
        />
        <small>留空表示不限速</small>
      </label>
      <div className="actions">
        <button type="button" onClick={save} disabled={busy}>
          保存限速间隔
        </button>
      </div>
      <p className="hint">价格范围、最大仓位、主仓比例、批数和网格参数改不了：要换就先停止，再新建。</p>
      {notice === null ? null : <p className="notice">{notice}</p>}
      {error === null ? null : <p className="error">{error}</p>}
    </section>
  )
}
