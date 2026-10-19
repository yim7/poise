// 运行中可以改的参数（设计第 2.4 节、B44）：每一侧的限速间隔和观点失效设置。
//
// 其他参数改不了：服务端会拒绝，并提示先停止再新建。这里只提交这两项。

import { useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'

import { Callout } from '@/components/layout'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { cn } from '@/lib/utils'
import { HttpError, api } from './api'
import {
  type InvalidationInput,
  invalidationInput,
  parseInvalidation,
  parseThrottle,
  throttleMinutes,
} from './format'
import { DIRECTION_LABELS } from './labels'
import { Panel } from './tables'
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
    return '正在执行上一步，稍后再试。'
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
      setNotice(`已生效（v${saved.version}）。`)
      await client.invalidateQueries({ queryKey: ['strategy', strategyId] })
    } catch (failure) {
      setError(describeSaveFailure(failure))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Panel
      title="运行中可改的参数"
      aside={
        <Button size="sm" onClick={save} disabled={busy}>
          保存运行参数
        </Button>
      }
    >
      <div className="flex flex-col gap-4 p-4">
        {sides.map((direction) => {
          const label = DIRECTION_LABELS[direction]
          const values = inputs[direction]
          return (
            <fieldset key={direction} className="grid min-w-0 grid-cols-2 gap-3 sm:grid-cols-4">
              <legend
                className={cn(
                  'mb-2 text-[11px] font-medium',
                  direction === 'long' ? 'text-long' : 'text-short',
                )}
              >
                {label}侧
              </legend>
              <Field
                label={`${label}侧限速间隔 τ（分钟）`}
                caption="限速 τ（分钟）"
                placeholder="不限速"
                value={values.throttleMinutes}
                disabled={busy}
                onChange={(value) => update(direction, { throttleMinutes: value })}
              />
              <Field
                label={`${label}侧观点失效价`}
                caption="观点失效价"
                placeholder="不止损"
                hint={direction === 'long' ? '须低于 L' : '须高于 H'}
                value={values.invalidation.price}
                disabled={busy}
                onChange={(value) => updateInvalidation(direction, { price: value })}
              />
              <Field
                label={`${label}侧失效后剩余数量 R`}
                caption="失效后剩余 R"
                hint={neutral ? '中性固定 0' : undefined}
                value={neutral ? '0' : values.invalidation.remaining}
                disabled={busy || neutral}
                onChange={(value) => updateInvalidation(direction, { remaining: value })}
              />
              <Field
                label={`${label}侧失效平仓滑点上限（%）`}
                caption="失效滑点上限 %"
                value={values.invalidation.slippage}
                disabled={busy}
                onChange={(value) => updateInvalidation(direction, { slippage: value })}
              />
            </fieldset>
          )
        })}
        <p className="text-[11px] text-muted-foreground">
          其他参数要停止后新建
        </p>
        {notice === null ? null : <Callout tone="ok">{notice}</Callout>}
        {error === null ? null : <Callout tone="danger">{error}</Callout>}
      </div>
    </Panel>
  )
}

function Field({
  label,
  caption,
  hint,
  placeholder,
  value,
  disabled,
  onChange,
}: {
  label: string
  caption: string
  hint?: string
  placeholder?: string
  value: string
  disabled: boolean
  onChange: (value: string) => void
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-[11px] text-muted-foreground">{caption}</span>
      <Input
        aria-label={label}
        className="num h-8"
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      />
      {hint === undefined ? null : <span className="text-[10px] text-muted-foreground/80">{hint}</span>}
    </label>
  )
}
