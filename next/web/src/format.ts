// 显示用的格式化。
//
// 小数是服务端算好的定点数，这里只做显示：不加、不减、不四舍五入成另一个数量。

import type { Direction, SideConfig, StrategyConfig, ThrottleInterval } from './types'

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
    return { problem: '限速间隔要填一个正数（分钟）；不限速就留空。' }
  }
  return { interval: [Math.round(parsed * 60), 0] }
}

/** 一侧参数的摘要。 */
export function sideSummary(side: SideConfig): string {
  const core =
    side.batch_count === null || Number(side.core_ratio) === 0
      ? '无主仓'
      : `主仓 ${side.core_ratio}×${side.batch_count} 批`
  return `${side.price_low}–${side.price_high}，间距 ${side.grid_spacing}，价差 ${side.grid_spread}，上限 ${side.max_position}，${core}`
}

/** 参数摘要：列表和版本记录里用一句话说清两侧的关键参数。 */
export function configSummary(config: StrategyConfig): string {
  const sides: [Direction, SideConfig][] = []
  if (config.long !== null) sides.push(['long', config.long])
  if (config.short !== null) sides.push(['short', config.short])
  return sides
    .map(([direction, side]) => `${direction === 'long' ? '做多' : '做空'} ${sideSummary(side)}`)
    .join('；')
}
