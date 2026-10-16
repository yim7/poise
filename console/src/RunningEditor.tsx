// 运行中可以改的参数（设计第 2.4 节、B44）：每一侧的限速间隔和观点失效设置。
//
// 其他参数改不了：服务端会拒绝，并提示先停止再新建。这里只提交这两项。

import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'

import { HttpError, api } from './api'
import {
  type InvalidationInput,
  invalidationInput,
  parseInvalidation,
  parseThrottle,
  throttleMinutes,
} from './format'
import { DIRECTION_LABELS } from './labels'
import type { Direction, StrategyConfig } from './types'

interface RunningEditorProps {
  strategyId: string
  config: StrategyConfig
}

/** 一侧能改的输入框。 */
interface SideInputs {
  throttleMinutes: string
  invalidation: InvalidationInput
}

/** 配置了的那几侧，按配置字段的顺序。 */
function configuredSides(config: StrategyConfig): Direction[] {
  const sides: Direction[] = []
  if (config.long !== null) sides.push('long')
  if (config.short !== null) sides.push('short')
  return sides
}

function sideInputs(config: StrategyConfig, direction: Direction): SideInputs {
  const side = config[direction]
  return {
    throttleMinutes: throttleMinutes(side?.throttle_interval ?? null),
    invalidation: invalidationInput(side?.invalidation ?? null),
  }
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

export function RunningEditor({ strategyId, config }: RunningEditorProps) {
  const [inputs, setInputs] = useState<Record<Direction, SideInputs>>({
    long: sideInputs(config, 'long'),
    short: sideInputs(config, 'short'),
  })
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const client = useQueryClient()
  const sides = configuredSides(config)
  const neutral = sides.length === 2

  function update(direction: Direction, change: Partial<SideInputs>) {
    setInputs((current) => ({ ...current, [direction]: { ...current[direction], ...change } }))
  }

  function updateInvalidation(direction: Direction, change: Partial<InvalidationInput>) {
    setInputs((current) => ({
      ...current,
      [direction]: {
        ...current[direction],
        invalidation: { ...current[direction].invalidation, ...change },
      },
    }))
  }

  async function save() {
    // 输入不合法就什么都不提交：把「写错了」当成「不限速」或「不止损」提交上去，
    // 用户以为设上了，实际提交的是另一份参数。
    const next: StrategyConfig = { ...config }
    for (const direction of sides) {
      const side = config[direction]
      if (side === null) continue
      const throttle = parseThrottle(inputs[direction].throttleMinutes)
      if ('problem' in throttle) {
        setError(`${DIRECTION_LABELS[direction]}侧：${throttle.problem}`)
        setNotice(null)
        return
      }
      const invalidation = parseInvalidation(inputs[direction].invalidation, neutral)
      if ('problem' in invalidation) {
        setError(`${DIRECTION_LABELS[direction]}侧：${invalidation.problem}`)
        setNotice(null)
        return
      }
      next[direction] = {
        ...side,
        throttle_interval: throttle.interval,
        invalidation: invalidation.invalidation,
      }
    }
    setBusy(true)
    setError(null)
    setNotice(null)
    try {
      const saved = await api.updateConfig(strategyId, next)
      setNotice(`运行参数已生效，记为第 ${saved.version} 版。`)
      await client.invalidateQueries({ queryKey: ['strategy', strategyId] })
    } catch (failure) {
      setError(describeSaveFailure(failure))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="running-editor">
      <h3>运行中可改的参数</h3>
      {sides.map((direction) => {
        const label = DIRECTION_LABELS[direction]
        const values = inputs[direction]
        return (
          <fieldset key={direction}>
            <legend>{label}侧</legend>
            <label className="field">
              <span>{label}侧限速间隔 τ（分钟）</span>
              <input
                aria-label={`${label}侧限速间隔 τ（分钟）`}
                value={values.throttleMinutes}
                disabled={busy}
                onChange={(event) => update(direction, { throttleMinutes: event.target.value })}
              />
              <small>留空表示不限速</small>
            </label>
            <label className="field">
              <span>{label}侧观点失效价</span>
              <input
                aria-label={`${label}侧观点失效价`}
                value={values.invalidation.price}
                disabled={busy}
                onChange={(event) => updateInvalidation(direction, { price: event.target.value })}
              />
              <small>
                {direction === 'long'
                  ? '留空表示不启用自动止损；必须低于 L'
                  : '留空表示不启用自动止损；必须高于 H'}
              </small>
            </label>
            <label className="field">
              <span>{label}侧失效后剩余数量 R</span>
              <input
                aria-label={`${label}侧失效后剩余数量 R`}
                value={neutral ? '0' : values.invalidation.remaining}
                disabled={busy || neutral}
                onChange={(event) => updateInvalidation(direction, { remaining: event.target.value })}
              />
              <small>
                {neutral ? '中性网格失效时全部平仓，固定为 0' : '0 表示全部平仓；不超过本侧主仓总量'}
              </small>
            </label>
            <label className="field">
              <span>{label}侧失效平仓滑点上限（%）</span>
              <input
                aria-label={`${label}侧失效平仓滑点上限（%）`}
                value={values.invalidation.slippage}
                disabled={busy}
                onChange={(event) => updateInvalidation(direction, { slippage: event.target.value })}
              />
            </label>
          </fieldset>
        )
      })}
      <div className="actions">
        <button type="button" onClick={save} disabled={busy}>
          保存运行参数
        </button>
      </div>
      <p className="hint">价格范围、最大仓位、主仓比例、批数和网格参数改不了：要换就先停止，再新建。</p>
      {notice === null ? null : <p className="notice">{notice}</p>}
      {error === null ? null : <p className="error">{error}</p>}
    </section>
  )
}
