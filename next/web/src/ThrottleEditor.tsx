// 运行中可以改的参数（设计第 2.4 节）：目前只有限速间隔，每一侧各一个。
//
// 其他参数改不了：服务端会拒绝，并提示先停止再新建。这里只提交限速这一项。

import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'

import { HttpError, api } from './api'
import { parseThrottle, throttleMinutes } from './format'
import { DIRECTION_LABELS } from './labels'
import type { Direction, StrategyConfig } from './types'

interface ThrottleEditorProps {
  strategyId: string
  config: StrategyConfig
}

/** 配置了的那几侧，按配置字段的顺序。 */
function configuredSides(config: StrategyConfig): Direction[] {
  const sides: Direction[] = []
  if (config.long !== null) sides.push('long')
  if (config.short !== null) sides.push('short')
  return sides
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
  const [minutes, setMinutes] = useState<Record<Direction, string>>({
    long: throttleMinutes(config.long?.throttle_interval ?? null),
    short: throttleMinutes(config.short?.throttle_interval ?? null),
  })
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const client = useQueryClient()
  const sides = configuredSides(config)

  async function save() {
    // 输入不合法就什么都不提交：把「写错了」当成「不限速」提交上去，用户以为限了速，
    // 实际提交的是另一份参数。
    const next: StrategyConfig = { ...config }
    for (const direction of sides) {
      const side = config[direction]
      if (side === null) continue
      const throttle = parseThrottle(minutes[direction])
      if ('problem' in throttle) {
        setError(`${DIRECTION_LABELS[direction]}侧：${throttle.problem}`)
        setNotice(null)
        return
      }
      next[direction] = { ...side, throttle_interval: throttle.interval }
    }
    setBusy(true)
    setError(null)
    setNotice(null)
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
      {sides.map((direction) => (
        <label className="field" key={direction}>
          <span>{DIRECTION_LABELS[direction]}侧限速间隔 τ（分钟）</span>
          <input
            aria-label={`${DIRECTION_LABELS[direction]}侧限速间隔 τ（分钟）`}
            value={minutes[direction]}
            disabled={busy}
            onChange={(event) =>
              setMinutes((current) => ({ ...current, [direction]: event.target.value }))
            }
          />
          <small>留空表示不限速</small>
        </label>
      ))}
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
