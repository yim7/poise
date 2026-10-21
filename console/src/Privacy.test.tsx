// 隐藏金额：顶栏开关遮住仓位、数量、盈亏和可用资金，价格照常显示；刷新后保持。

import { afterEach, describe, expect, it } from 'vitest'
import { screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { CloseDialog } from '@/components/close-dialog'
import { MASK, PrivacyProvider, PrivacyToggle, maskNumbers } from '@/components/privacy'
import { configSummary } from './format'
import { PreviewPanel } from './PreviewPanel'
import { StrategyDetailView } from './StrategyDetail'
import { StrategyList } from './StrategyList'
import { fakeServer } from './test/fakeServer'
import { config, detail, longSide, preview, summary } from './test/fixtures'
import { renderWithQuery } from './test/render'

function withToggle(ui: React.ReactNode) {
  return renderWithQuery(
    <PrivacyProvider>
      <PrivacyToggle />
      {ui}
    </PrivacyProvider>,
  )
}

afterEach(() => window.localStorage.clear())

describe('隐藏金额', () => {
  it('运行详情：遮住仓位、盈亏、可用资金和表里的数量，价格照常显示', async () => {
    fakeServer({ 'GET /api/strategies/s-1': { body: detail() } })
    withToggle(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const readings = await screen.findByRole('region', { name: '关键读数' })
    expect(within(readings).getByText('0.015')).toBeInTheDocument()
    expect(within(readings).getByText('100000')).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '隐藏金额' }))

    // 交易所仓位、浮动盈亏、合计、网格利润、累计资金费、可用 USDC。
    expect(within(readings).getAllByText(MASK)).toHaveLength(6)
    for (const value of ['0.015', '22.5', '51', '30', '-1.5', '100000']) {
      expect(within(readings).queryByText(value)).not.toBeInTheDocument()
    }
    // 价格和比率推不出资金量，照常显示。
    expect(within(readings).getByText('94000')).toBeInTheDocument()
    expect(within(readings).getByText('92100')).toBeInTheDocument()
    expect(within(readings).getByText('1.23%')).toBeInTheDocument()

    // 格子、批次、挂单里的数量逐项消失，价格照常显示。
    const ladder = screen.getByRole('region', { name: '网格格子' })
    expect(within(ladder).queryByText('0.015')).not.toBeInTheDocument()
    expect(within(ladder).getByText('94000')).toBeInTheDocument()
    const batchTable = screen.getByRole('region', { name: '主仓批次' })
    for (const value of ['0.175', '0.075']) {
      expect(within(batchTable).queryByText(value)).not.toBeInTheDocument()
      expect(within(batchTable).queryByText(`/ ${value}`, { exact: false })).not.toBeInTheDocument()
    }
    const orders = screen.getByRole('region', { name: '在场挂单' })
    expect(within(orders).queryByText('0.175')).not.toBeInTheDocument()
    expect(within(orders).getByText('95000')).toBeInTheDocument()
    // 参数摘要里的持仓上限。
    const params = screen.getByRole('region', { name: '参数' })
    expect(within(params).getByText(/上限 ••••/)).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '显示金额' }))
    expect(within(readings).getByText('0.015')).toBeInTheDocument()
    expect(within(readings).queryByText(MASK)).not.toBeInTheDocument()
  })

  it('策略列表：卡片的净仓位和草稿摘要里的上限被遮住；重新打开页面仍然隐藏', async () => {
    fakeServer({
      'GET /api/strategies': { body: [summary()] },
      'GET /api/drafts': {
        body: [{ draft_id: 's-3', config, created_at: '2026-10-15T00:00:00Z', updated_at: '2026-10-15T00:00:00Z' }],
      },
    })
    const list = <StrategyList onOpenStrategy={() => {}} onOpenDraft={() => {}} onNew={() => {}} onOpenSettings={() => {}} />
    const first = withToggle(list)
    await screen.findByText('0.015')
    await userEvent.click(screen.getByRole('button', { name: '隐藏金额' }))
    expect(screen.queryByText('0.015')).not.toBeInTheDocument()
    expect(screen.getByText(/上限 ••••/)).toBeInTheDocument()
    first.unmount()

    // 开关记在本机：刷新之后还是隐藏的。
    withToggle(list)
    expect(await screen.findByRole('button', { name: '显示金额' })).toBeInTheDocument()
    expect(await screen.findByText(/上限 ••••/)).toBeInTheDocument()
    expect(screen.queryByText('0.015')).not.toBeInTheDocument()
  })

  it('服务端说明里的数量：隐藏时除百分比以外的数都遮住', async () => {
    expect(maskNumbers('做多侧满仓 0.87654、价格到 75000 时保证金率 36%，超过提醒阈值 30.5%。')).toBe(
      `做多侧满仓 ${MASK}、价格到 ${MASK} 时保证金率 36%，超过提醒阈值 30.5%。`,
    )

    fakeServer({
      'GET /api/strategies/s-1': {
        body: detail({ summary: summary({ status: 'closing' }), note: '平仓中：还要平 0.123456。' }),
      },
    })
    withToggle(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    expect(await screen.findByText('平仓中：还要平 0.123456。')).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '隐藏金额' }))
    expect(screen.getByText(`平仓中：还要平 ${MASK}。`)).toBeInTheDocument()
  })

  it('预览：仓位、可用资金、满仓数量、校验原因和提醒里的数量都遮住', async () => {
    window.localStorage.setItem('poise.hide-amounts', '1')
    withToggle(
      <PreviewPanel
        config={config}
        preview={preview({
          position: '0.3',
          validation: { valid: false, reason: '标的上已有多仓 1.23456，超过做多侧最大仓位 M 1' },
          warnings: ['建议最大仓位 M ≤ 0.76543（保证金率不超过 30%）。'],
        })}
      />,
    )
    expect(screen.getByText(`标的上已有多仓 ${MASK}，超过做多侧最大仓位 M ${MASK}`)).toBeInTheDocument()
    expect(screen.getByText(`建议最大仓位 M ≤ ${MASK}（保证金率不超过 30%）。`)).toBeInTheDocument()
    for (const value of ['0.3', '100000']) expect(screen.queryByText(value)).not.toBeInTheDocument()
    const stress = within(screen.getByRole('region', { name: '满仓压力测试' }))
    expect(stress.queryByText('1')).not.toBeInTheDocument()
    expect(stress.getByText('75000')).toBeInTheDocument()
  })

  it('平仓确认弹窗的净仓位和参数摘要里的失效剩余数量都遮住', () => {
    window.localStorage.setItem('poise.hide-amounts', '1')
    withToggle(<CloseDialog open onOpenChange={() => {}} position="0.42" mark="95000" onConfirm={() => {}} />)
    expect(screen.queryByText('0.42')).not.toBeInTheDocument()
    expect(screen.getByText(MASK)).toBeInTheDocument()

    const invalidating = {
      ...config,
      long: { ...longSide, invalidation: { price: '70000', remaining: '0.4', max_slippage: '0.01' } },
    }
    expect(configSummary(invalidating, true)).toContain(`失效价 70000（剩 ${MASK}，`)
    expect(configSummary(invalidating, true)).not.toContain('0.4')
  })
})
