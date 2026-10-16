// 账户风控设置页（第 7.4、11 节）：显示和修改提醒阈值、停止阈值、行情过期时间。

import { describe, expect, it } from 'vitest'
import { screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { AccountSettingsView } from './AccountSettings'
import { percentInputToRatio, ratioToPercentInput } from './format'
import { fakeServer } from './test/fakeServer'
import { renderWithQuery } from './test/render'

const defaults = {
  margin_alert_ratio: '0.3',
  margin_stop_ratio: '0.5',
  market_data_max_age: 10,
  funding_alert_annual_rate: '0.8',
}

describe('账户风控设置', () => {
  it('显示当前设置，改完保存时把百分比换成比率交给服务端', async () => {
    const server = fakeServer({
      'GET /api/account/settings': { body: defaults },
      'PUT /api/account/settings': {
        body: { ...defaults, margin_alert_ratio: '0.25' },
      },
    })
    renderWithQuery(<AccountSettingsView onBack={() => {}} />)

    const alert = await screen.findByDisplayValue('30')
    expect(screen.getByDisplayValue('50')).toBeInTheDocument()
    expect(screen.getByDisplayValue('10')).toBeInTheDocument()
    expect(screen.getByDisplayValue('80')).toBeInTheDocument()

    await userEvent.clear(alert)
    await userEvent.type(alert, '25')
    await userEvent.click(screen.getByRole('button', { name: '保存' }))

    await screen.findByText(/已保存/)
    expect(server.lastBody('PUT', '/api/account/settings')).toEqual({
      margin_alert_ratio: '0.25',
      margin_stop_ratio: '0.5',
      market_data_max_age: 10,
      funding_alert_annual_rate: '0.8',
    })
  })

  it('服务端说设置不合理时原样显示原因', async () => {
    fakeServer({
      'GET /api/account/settings': { body: defaults },
      'PUT /api/account/settings': {
        status: 400,
        body: { kind: 'invalid_config', error: 'the margin alert ratio 0.6 must be below the stop ratio 0.5' },
      },
    })
    renderWithQuery(<AccountSettingsView onBack={() => {}} />)

    const alert = await screen.findByDisplayValue('30')
    await userEvent.clear(alert)
    await userEvent.type(alert, '60')
    await userEvent.click(screen.getByRole('button', { name: '保存' }))

    await waitFor(() => {
      expect(screen.getByText(/must be below the stop ratio/)).toBeInTheDocument()
    })
  })

  it('不是数字的阈值不交给服务端', async () => {
    const server = fakeServer({ 'GET /api/account/settings': { body: defaults } })
    renderWithQuery(<AccountSettingsView onBack={() => {}} />)

    const alert = await screen.findByDisplayValue('30')
    await userEvent.clear(alert)
    await userEvent.type(alert, '三十')
    await userEvent.click(screen.getByRole('button', { name: '保存' }))

    expect(screen.getByText(/百分比数字/)).toBeInTheDocument()
    expect(server.count('PUT', '/api/account/settings')).toBe(0)
  })

  it('保存之后又改了输入，不再说已保存', async () => {
    fakeServer({
      'GET /api/account/settings': { body: defaults },
      'PUT /api/account/settings': { body: defaults },
    })
    renderWithQuery(<AccountSettingsView onBack={() => {}} />)

    const stop = await screen.findByDisplayValue('50')
    await userEvent.click(screen.getByRole('button', { name: '保存' }))
    await screen.findByText(/已保存/)

    await userEvent.type(stop, '0')
    expect(screen.queryByText(/已保存/)).not.toBeInTheDocument()
  })

  it('秒数不是整数字符串时不交给服务端', async () => {
    const server = fakeServer({ 'GET /api/account/settings': { body: defaults } })
    renderWithQuery(<AccountSettingsView onBack={() => {}} />)

    const maxAge = await screen.findByDisplayValue('10')
    await userEvent.clear(maxAge)
    await userEvent.type(maxAge, '1.0000000000000001')
    await userEvent.click(screen.getByRole('button', { name: '保存' }))

    expect(screen.getByText(/正整数秒/)).toBeInTheDocument()
    expect(server.count('PUT', '/api/account/settings')).toBe(0)
  })

  it('百分比和比率之间只移动小数点', () => {
    expect(percentInputToRatio('30')).toBe('0.3')
    expect(percentInputToRatio('12.5')).toBe('0.125')
    expect(percentInputToRatio('100')).toBe('1')
    expect(percentInputToRatio('0.05')).toBe('0.0005')
    expect(percentInputToRatio('abc')).toBeNull()
    expect(ratioToPercentInput('0.3')).toBe('30')
    expect(ratioToPercentInput('0.125')).toBe('12.5')
  })
})
