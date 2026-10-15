// 列表页：策略和草稿都来自服务端汇总，页面不自己算。

import { describe, expect, it, vi } from 'vitest'
import { screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { StrategyList } from './StrategyList'
import { fakeServer } from './test/fakeServer'
import { config, summary } from './test/fixtures'
import { renderWithQuery } from './test/render'

function listRoutes() {
  return fakeServer({
    'GET /api/strategies': {
      body: [
        summary(),
        summary({ strategy_id: 's-2', status: 'stopped', position: '0', working_orders: 0 }),
      ],
    },
    'GET /api/drafts': {
      body: [
        {
          draft_id: 's-3',
          config,
          created_at: '2026-10-15T00:00:00Z',
          updated_at: '2026-10-15T00:00:00Z',
        },
      ],
    },
  })
}

describe('策略列表', () => {
  it('显示标的、状态、净仓位和在场挂单，草稿单独一栏', async () => {
    listRoutes()
    renderWithQuery(
      <StrategyList onNew={() => {}} onOpenDraft={() => {}} onOpenStrategy={() => {}} />,
    )

    expect(await screen.findByText('运行中')).toBeInTheDocument()
    expect(screen.getByText('已归档')).toBeInTheDocument()
    expect(screen.getByText('0.015')).toBeInTheDocument()
    // 「草稿」既是分栏标题，也是草稿行的状态。
    expect(screen.getAllByText('草稿')).toHaveLength(2)
    expect(screen.getByText(/75000–95000/)).toBeInTheDocument()
  })

  it('点“查看”打开运行详情，点“新建策略”打开新建表单', async () => {
    listRoutes()
    const openStrategy = vi.fn()
    const onNew = vi.fn()
    renderWithQuery(
      <StrategyList onNew={onNew} onOpenDraft={() => {}} onOpenStrategy={openStrategy} />,
    )

    await userEvent.click((await screen.findAllByRole('button', { name: '查看' }))[0])
    expect(openStrategy).toHaveBeenCalledWith('s-1')

    await userEvent.click(screen.getByRole('button', { name: '新建策略' }))
    expect(onNew).toHaveBeenCalled()
  })

  it('服务端读不到时把原因显示出来，而不是空着', async () => {
    fakeServer({
      'GET /api/strategies': { status: 502, body: { error: 'exchange unreachable' } },
      'GET /api/drafts': { body: [] },
    })
    renderWithQuery(
      <StrategyList onNew={() => {}} onOpenDraft={() => {}} onOpenStrategy={() => {}} />,
    )

    await waitFor(() => {
      expect(screen.getByText(/exchange unreachable/)).toBeInTheDocument()
    })
  })
})
