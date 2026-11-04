// 显示用的格式化：只换写法，不改数。

import { describe, expect, it } from 'vitest'

import { percent, sideSummary } from './format'
import { coreSide } from './test/fixtures'

describe('百分比', () => {
  it('比率按百分比显示', () => {
    expect(percent('0.3')).toBe('30%')
    expect(percent('0.0000125')).toBe('0.00125%')
    expect(percent(null)).toBe('读不到')
  })

  it('负数保留负号：负资金费率是空头付，丢了负号就说反了', () => {
    expect(percent('-0.00027774')).toBe('-0.027774%')
    expect(percent('-0.005')).toBe('-0.5%')
    expect(percent('-2.4330532')).toBe('-243.30532%')
    expect(percent('-0')).toBe('0%')
  })
})

describe('一侧参数的摘要', () => {
  it('有主仓倾斜时在批数后面写出来，没有就不写', () => {
    expect(sideSummary(coreSide)).toMatch(/主仓 0\.7×4 批$/)
    expect(sideSummary({ ...coreSide, core_skew: '3' })).toContain('主仓 0.7×4 批（倾斜 3）')
  })
})
