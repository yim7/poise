// 显示用的格式化。
//
// 小数是服务端算好的定点数，这里只做显示：不加、不减、不四舍五入成另一个数量。

import { MASK } from '@/components/privacy'
import type { Direction, Invalidation, SideConfig, StrategyConfig, ThrottleInterval } from './types'

/** 原样显示服务端的定点小数。 */
export function decimal(value: string | null | undefined): string {
  return value === null || value === undefined || value === '' ? '—' : value
}

/** 金额按 USDC 显示到分：小数第三位起截掉，不做浮点运算。 */
export function usdc(value: string): string {
  const [whole, fraction = ''] = value.split('.')
  const cents = fraction.slice(0, 2).replace(/0+$/, '')
  return cents === '' ? whole : `${whole}.${cents}`
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

/** 失效设置输入框里的值。 */
export interface InvalidationInput {
  price: string
  /** 失效后保留主仓的百分比；留空按 0，全部平仓。 */
  keepPercent: string
  /** 滑点上限，百分比。 */
  slippage: string
  /**
   * 旧配置直接按数量写的 R（没有比例）。百分比留空时原样提交，不把它改成别的数；
   * 填了百分比就按百分比。
   */
  legacyRemaining: string | null
}

/** 失效设置在输入框里的写法；没启用时滑点上限给默认的 1%。 */
export function invalidationInput(invalidation: Invalidation | null): InvalidationInput {
  if (invalidation === null) return { price: '', keepPercent: '', slippage: '1', legacyRemaining: null }
  const slippage = ratioToPercentInput(invalidation.max_slippage)
  if (invalidation.remaining_ratio !== null) {
    return {
      price: invalidation.price,
      keepPercent: ratioToPercentInput(invalidation.remaining_ratio),
      slippage,
      legacyRemaining: null,
    }
  }
  // 旧配置：剩余 0 就是保留 0%，显示成 0；不是 0 的数量换不成准确的百分比，留空。两种都记下
  // 原来的数量，没改的时候原样提交，不平白多出一个参数版本。
  const zero = Number(invalidation.remaining) === 0
  return {
    price: invalidation.price,
    keepPercent: zero ? '0' : '',
    slippage,
    legacyRemaining: invalidation.remaining,
  }
}

/**
 * 输入框里的失效设置换成提交的参数。失效价留空就是不启用；保留主仓的百分比留空按 0；
 * 中性网格按 0 提交（任一侧失效都全部平仓，第 7.5 节）。百分比只移动小数点换成比率，
 * R 由服务端按主仓总量算出。价格和比例是否合理由服务端校验。
 */
export function parseInvalidation(
  input: InvalidationInput,
  neutral: boolean,
): { invalidation: Invalidation | null } | { problem: string } {
  const price = input.price.trim()
  if (price === '') return { invalidation: null }
  const slippage = percentInputToRatio(input.slippage)
  if (slippage === null) return { problem: '失效滑点上限填百分比数字。' }
  const keep = input.keepPercent.trim()
  if (!neutral && keep === '' && input.legacyRemaining !== null) {
    return {
      invalidation: { price, remaining: input.legacyRemaining, remaining_ratio: null, max_slippage: slippage },
    }
  }
  const ratio = neutral || keep === '' ? '0' : percentInputToRatio(keep)
  if (ratio === null) return { problem: '失效后保留主仓填百分比数字。' }
  if (ratio === '0' && input.legacyRemaining !== null && Number(input.legacyRemaining) === 0) {
    return { invalidation: { price, remaining: '0', remaining_ratio: null, max_slippage: slippage } }
  }
  return {
    invalidation: { price, remaining: '0', remaining_ratio: ratio, max_slippage: slippage },
  }
}

/** 一侧参数的摘要。`hideAmounts` 时遮住持仓上限和失效剩余数量（见 `components/privacy`）。 */
export function sideSummary(side: SideConfig, hideAmounts = false): string {
  const amount = (value: string) => (hideAmounts ? MASK : value)
  const core =
    side.batch_count === null || Number(side.core_ratio) === 0
      ? '无主仓'
      : `主仓 ${side.core_ratio}×${side.batch_count} 批${side.core_skew === null ? '' : `（倾斜 ${side.core_skew}）`}`
  // 按比例设置的 R 只报比例：草稿里的 R 还没算出来，报数量会是 0。
  const remaining = (invalidation: Invalidation) =>
    invalidation.remaining_ratio === null
      ? `剩 ${amount(invalidation.remaining)}`
      : `留主仓 ${percent(invalidation.remaining_ratio)}`
  const invalidation =
    side.invalidation === null
      ? ''
      : `，失效价 ${side.invalidation.price}（${remaining(side.invalidation)}，滑点 ≤${percent(side.invalidation.max_slippage)}）`
  const grid =
    side.grid_count === null
      ? `间距 ${side.grid_spacing}，价差 ${side.grid_spread}`
      : `${side.grid_count} 格，价差 ${side.grid_spread_ratio ?? '1'}×间距`
  const size = side.capital === null ? `上限 ${amount(side.max_position)}` : `投入 ${amount(side.capital)}`
  return `${side.price_low}–${side.price_high}，${grid}，${size}，${core}${invalidation}`
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
