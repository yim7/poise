// 显示用的格式化。
//
// 小数是服务端算好的定点数，这里只做显示：不加、不减、不四舍五入成另一个数量。

import { MASK } from '@/components/privacy'
import type { Direction, Invalidation, SideConfig, StrategyConfig, ThrottleInterval } from './types'

/** 原样显示服务端的定点小数。 */
export function decimal(value: string | null | undefined): string {
  return value === null || value === undefined || value === '' ? '—' : value
}

/**
 * 比率按百分比显示，例如 0.3 显示成 30%。只换写法，不改数：小数点右移两位。
 * 没读到时（null）显示「读不到」。
 */
export function percent(value: string | null): string {
  if (value === null) return '读不到'
  // 符号单独处理：资金费率可以是负的，`Number('-000')` 会把负号吞掉。
  if (value.startsWith('-')) {
    const magnitude = percent(value.slice(1))
    return magnitude === '0%' ? magnitude : `-${magnitude}`
  }
  const [whole, fraction = ''] = value.split('.')
  const padded = fraction.padEnd(2, '0')
  const integer = String(Number(`${whole}${padded.slice(0, 2)}`))
  const rest = padded.slice(2).replace(/0+$/, '')
  return rest === '' ? `${integer}%` : `${integer}.${rest}%`
}

/**
 * 比率换成百分比输入框里的数，例如 0.3 → 30。只移动小数点，不做浮点运算。
 */
export function ratioToPercentInput(ratio: string): string {
  return percent(ratio).replace('%', '')
}

/**
 * 百分比输入换回比率，例如 30 → 0.3、12.5 → 0.125。只移动小数点，不做浮点运算；
 * 不是非负小数时返回 null。
 */
export function percentInputToRatio(input: string): string | null {
  const text = input.trim()
  if (!/^\d+(\.\d+)?$/.test(text)) return null
  const [whole, fraction = ''] = text.split('.')
  const padded = whole.padStart(3, '0')
  const integer = padded.slice(0, padded.length - 2).replace(/^0+(?=\d)/, '')
  const decimals = `${padded.slice(padded.length - 2)}${fraction}`.replace(/0+$/, '')
  return decimals === '' ? integer : `${integer}.${decimals}`
}

/** 本地时间；服务端给的是 UTC 的 ISO 串。 */
export function timestamp(iso: string): string {
  const at = new Date(iso)
  return Number.isNaN(at.getTime()) ? iso : at.toLocaleString('zh-CN', { hour12: false })
}

/**
 * 限速间隔按分钟显示：界面上一律用分钟填写。
 *
 * 不取整：间隔不一定是整分钟，取整会让「不改这一项直接保存」把服务端的值改掉。
 */
export function throttleMinutes(interval: ThrottleInterval): string {
  if (interval === null) return ''
  const [seconds] = interval
  return String(seconds / 60)
}

/** 分钟输入的解析结果：要么是一个限速间隔，要么是一句说给用户听的话。 */
export type ThrottleInput = { interval: ThrottleInterval } | { problem: string }

/**
 * 把分钟输入变成限速间隔。
 *
 * 留空是「不限速」的唯一写法。填了别的（写了字、填了零或负数）不算不限速：那样会把
 * 用户填的东西悄悄换掉，提交上去的是另一份参数。这种情况返回原因，由调用方去提示。
 */
export function parseThrottle(minutes: string): ThrottleInput {
  const value = minutes.trim()
  if (value === '') return { interval: null }
  const parsed = Number(value)
  if (!Number.isFinite(parsed) || parsed <= 0) {
    return { problem: '限速间隔填正数（分钟），留空不限速。' }
  }
  return { interval: [Math.round(parsed * 60), 0] }
}

/** 失效设置输入框里的三个值。 */
export interface InvalidationInput {
  price: string
  remaining: string
  /** 滑点上限，百分比。 */
  slippage: string
}

/** 失效设置在输入框里的写法；没启用时滑点上限给默认的 1%。 */
export function invalidationInput(invalidation: Invalidation | null): InvalidationInput {
  if (invalidation === null) return { price: '', remaining: '', slippage: '1' }
  return {
    price: invalidation.price,
    remaining: invalidation.remaining,
    slippage: ratioToPercentInput(invalidation.max_slippage),
  }
}

/**
 * 输入框里的失效设置换成提交的参数。失效价留空就是不启用；剩余数量留空按 0；中性网格
 * 剩余数量按 0 提交（任一侧失效都全部平仓，第 7.5 节）。滑点上限填的是百分比，只移动
 * 小数点换成比率。价格和数量是否合理由服务端校验。
 */
export function parseInvalidation(
  input: InvalidationInput,
  neutral: boolean,
): { invalidation: Invalidation | null } | { problem: string } {
  const price = input.price.trim()
  if (price === '') return { invalidation: null }
  const slippage = percentInputToRatio(input.slippage)
  if (slippage === null) return { problem: '失效滑点上限填百分比数字。' }
  const remaining = input.remaining.trim()
  return {
    invalidation: {
      price,
      remaining: neutral || remaining === '' ? '0' : remaining,
      max_slippage: slippage,
    },
  }
}

/** 一侧参数的摘要。`hideAmounts` 时遮住持仓上限和失效剩余数量（见 `components/privacy`）。 */
export function sideSummary(side: SideConfig, hideAmounts = false): string {
  const amount = (value: string) => (hideAmounts ? MASK : value)
  const core =
    side.batch_count === null || Number(side.core_ratio) === 0
      ? '无主仓'
      : `主仓 ${side.core_ratio}×${side.batch_count} 批`
  const invalidation =
    side.invalidation === null
      ? ''
      : `，失效价 ${side.invalidation.price}（剩 ${amount(side.invalidation.remaining)}，滑点 ≤${percent(side.invalidation.max_slippage)}）`
  return `${side.price_low}–${side.price_high}，间距 ${side.grid_spacing}，价差 ${side.grid_spread}，上限 ${amount(side.max_position)}，${core}${invalidation}`
}

/** 参数摘要：列表和版本记录里用一句话说清两侧的关键参数。 */
export function configSummary(config: StrategyConfig, hideAmounts = false): string {
  const sides: [Direction, SideConfig][] = []
  if (config.long !== null) sides.push(['long', config.long])
  if (config.short !== null) sides.push(['short', config.short])
  return sides
    .map(([direction, side]) => `${direction === 'long' ? '做多' : '做空'} ${sideSummary(side, hideAmounts)}`)
    .join('；')
}
