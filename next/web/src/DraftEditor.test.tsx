// 新建、预览、确认启动（B42、B43、B47）。
//
// 页面只发请求、只显示服务端给的数据：数量、批次和格子的划分都在服务端算好。

import { describe, expect, it, vi } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { DraftEditor } from './DraftEditor'
import { fakeServer, type FakeServer, type Routes } from './test/fakeServer'
import { neutralPreview, preview } from './test/fixtures'
import { renderWithQuery } from './test/render'

function editorRoutes(extra: Routes = {}) {
  return fakeServer({
    'POST /api/drafts': { body: { draft_id: 's-1' } },
    'PUT /api/drafts/s-1': { body: { version: 2 } },
    'POST /api/drafts/s-1/preview': { body: preview() },
    'POST /api/drafts/s-1/start': { body: { strategy_id: 's-1' } },
    ...extra,
  })
}

/** 一侧的字段：两侧的字段标签一样，按分组取才不会撞上。 */
function side(name: string) {
  return within(screen.getByRole('group', { name }))
}

function renderEditor(server: FakeServer, onStarted = vi.fn()) {
  renderWithQuery(<DraftEditor draftId={null} onBack={() => {}} onStarted={onStarted} />)
  return { server, onStarted }
}

/** 保存并预览，等到预览出来。 */
async function saveAndPreview() {
  await userEvent.click(screen.getByRole('button', { name: '保存并预览' }))
  await screen.findByText('参数校验通过。')
}

describe('新建与确认启动', () => {
  it('b42_确认启动之前不下任何单：没有预览时确认按钮是禁用的', async () => {
    const server = editorRoutes()
    renderEditor(server)

    expect(screen.getByRole('button', { name: '确认启动' })).toBeDisabled()

    await saveAndPreview()

    expect(server.count('POST', '/api/drafts/s-1/start')).toBe(0)
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()
  })

  it('b42_保存之后能看到批次表、格子表和参数版本，确认启动带上预览令牌', async () => {
    const server = editorRoutes()
    const { onStarted } = renderEditor(server)

    await saveAndPreview()

    expect(screen.getByText('主仓批次表')).toBeInTheDocument()
    expect(screen.getByText('网格格子表')).toBeInTheDocument()
    // 每格扣费后收益由服务端算好，页面直接显示。
    expect(screen.getByText('14.57025')).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    await waitFor(() => {
      expect(onStarted).toHaveBeenCalledWith('s-1')
    })
    expect(server.lastBody('POST', '/api/drafts/s-1/start')).toEqual({ token: 'token-1' })
    // 保存记一版，启动用那一版的预览。默认只配多头侧，空头侧是 null。
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      instrument: 'BTC',
      long: { price_low: '75000', price_high: '95000', core_ratio: '0' },
      short: null,
    })
  })

  it('b43_改过参数之后旧预览作废：确认启动重新变回禁用', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await saveAndPreview()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()

    const spacing = side('做多侧').getByLabelText('网格间距 d')
    await userEvent.clear(spacing)
    await userEvent.type(spacing, '500')

    expect(screen.getByRole('button', { name: '确认启动' })).toBeDisabled()
  })

  it('做空网格：只勾空头侧，提交的配置里没有多头侧', async () => {
    const server = editorRoutes()
    renderEditor(server)

    // 默认只有多头侧：把它取消，勾上空头侧。
    await userEvent.click(screen.getByLabelText('启用做多侧'))
    await userEvent.click(screen.getByLabelText('启用做空侧'))
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      instrument: 'BTC',
      long: null,
      short: { price_low: '96000', price_high: '99000', core_ratio: '0' },
    })
    expect(screen.getByText('网格格子表')).toBeInTheDocument()
  })

  it('中性网格：两侧都勾上，主仓比例按 0 提交且改不了', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': { body: neutralPreview() },
    })
    renderEditor(server)

    await userEvent.click(screen.getByLabelText('启用做空侧'))
    await saveAndPreview()

    // 两侧都配置就是中性网格：两侧都不设主仓，输入框也改不了。
    expect(side('做多侧').getByLabelText('主仓比例 r')).toBeDisabled()
    expect(side('做空侧').getByLabelText('主仓比例 r')).toBeDisabled()
    expect(screen.getByText('两侧都配置：这是中性网格，两侧都不设主仓。')).toBeInTheDocument()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      instrument: 'BTC',
      long: { price_low: '75000', core_ratio: '0', batch_count: null },
      short: { price_low: '96000', core_ratio: '0', batch_count: null },
    })

    // 预览里的格子表按侧展示：两侧各一格。
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(3)
    expect(rows[1]).toHaveTextContent('做多')
    expect(rows[1]).toHaveTextContent('89')
    expect(rows[2]).toHaveTextContent('做空')
    expect(rows[2]).toHaveTextContent('91')
  })

  it('b43_服务端说预览过期时，要求重新预览而不是照旧启动', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/start': {
        status: 409,
        body: { kind: 'preview_stale', error: 'the market moved since the preview; preview again' },
      },
    })
    renderEditor(server)

    await saveAndPreview()
    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    expect(await screen.findByText(/请重新预览/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeDisabled()
  })

  it('b46_标的有别的策略在跑时说清下一步，而不是让人重新预览', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/start': {
        status: 409,
        body: {
          kind: 'instrument_busy',
          error: 'instrument `BTC` already has a live strategy `s-2`',
        },
      },
    })
    renderEditor(server)

    await saveAndPreview()
    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    expect(await screen.findByText(/先停掉那一个/)).toBeInTheDocument()
    // 预览没作废：停掉那一个之后还能用这次预览启动。
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()
  })

  it('服务端正忙时说稍后再点，也不作废预览', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/start': {
        status: 409,
        body: { kind: 'busy', error: 'strategy `s-1` is already being driven' },
      },
    })
    renderEditor(server)

    await saveAndPreview()
    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    expect(await screen.findByText(/稍等一下再点确认启动/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()
  })

  it('b21_标的上已有仓位时，预览列出第一步挂出的订单和交易所上已有的其他挂单', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': {
        body: preview({
          position: '0.37',
          planned_orders: [
            {
              unit: { batch: ['long', 1] },
              purpose: 'core',
              side: 'buy',
              limit_price: '88999',
              quantity: '0.091',
              reduce_only: false,
            },
            {
              unit: { cell: ['long', 7] },
              purpose: 'grid_exit',
              side: 'sell',
              limit_price: '89000',
              quantity: '0.006',
              reduce_only: true,
            },
          ],
          foreign_orders: [
            {
              client_order_id: null,
              exchange_order_id: '777',
              side: 'sell',
              limit_price: '120000',
              quantity: '0.01',
              cumulative_filled: '0',
              reduce_only: false,
              status: 'open',
            },
          ],
        }),
      },
    })
    renderEditor(server)

    await saveAndPreview()

    expect(screen.getByText(/标的上已有仓位/)).toBeInTheDocument()
    const planned = screen.getByRole('table', { name: '启动后第一步挂出的订单' })
    // 限价由服务端算好（已避开自己的卖单），页面原样显示。
    expect(within(planned).getByText('88999')).toBeInTheDocument()
    expect(within(planned).getByText('0.091')).toBeInTheDocument()
    const foreign = screen.getByRole('table', { name: '交易所上已有的其他挂单（本系统不撤销）' })
    expect(within(foreign).getByText('777（手动下单）')).toBeInTheDocument()
  })

  it('参数校验不通过时只显示原因，不给确认启动的机会', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': {
        body: preview({
          validation: { valid: false, reason: 'the grid spread is too small' },
          token: null,
        }),
      },
    })
    renderEditor(server)

    await userEvent.click(screen.getByRole('button', { name: '保存并预览' }))

    expect(await screen.findByText(/the grid spread is too small/)).toBeInTheDocument()
    expect(screen.queryByText('网格格子表')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeDisabled()
  })

  it('b47_表单里没有私钥输入框，页面上也不出现私钥', async () => {
    editorRoutes()
    const { container } = renderWithQuery(
      <DraftEditor draftId={null} onBack={() => {}} onStarted={() => {}} />,
    )

    const text = container.textContent ?? ''
    for (const forbidden of ['私钥', 'private', 'secret', 'api_wallet']) {
      expect(text.toLowerCase()).not.toContain(forbidden.toLowerCase())
    }
    expect(container.querySelectorAll('input[type="password"]')).toHaveLength(0)
    const names = Array.from(container.querySelectorAll('input, select')).map(
      (element) => element.getAttribute('name') ?? '',
    )
    expect(names.some((name) => name.includes('key'))).toBe(false)
  })
})
