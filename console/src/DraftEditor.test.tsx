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
  await screen.findByText('校验通过')
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

  it('新建的草稿第一次保存后报出草稿标识，页面据此换成草稿地址，刷新还能回到这份草稿', async () => {
    const server = editorRoutes()
    const onCreated = vi.fn()
    renderWithQuery(
      <DraftEditor draftId={null} onBack={() => {}} onStarted={() => {}} onCreated={onCreated} />,
    )

    await saveAndPreview()
    expect(onCreated).toHaveBeenCalledWith('s-1')

    // 再保存是改同一份草稿，不再报新标识。
    await userEvent.click(screen.getByRole('button', { name: '保存' }))
    await waitFor(() => {
      expect(server.count('PUT', '/api/drafts/s-1')).toBe(1)
    })
    expect(onCreated).toHaveBeenCalledTimes(1)
  })

  it('网格价差默认跟随间距：改间距时价差一起变', async () => {
    const server = editorRoutes()
    renderEditor(server)

    const spacing = side('做多侧').getByLabelText('网格间距 d')
    await userEvent.clear(spacing)
    await userEvent.type(spacing, '500')

    expect(side('做多侧').getByLabelText('网格价差 g')).toHaveValue('500')
    expect(side('做多侧').getByText('= d')).toBeInTheDocument()
    await saveAndPreview()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { grid_spacing: '500', grid_spread: '500' },
    })
  })

  it('改过价差就记下它和间距的比例：之后改间距，价差按比例跟着变', async () => {
    const server = editorRoutes()
    renderEditor(server)

    const spread = side('做多侧').getByLabelText('网格价差 g')
    await userEvent.clear(spread)
    await userEvent.type(spread, '2000')
    expect(side('做多侧').getByText('= 2 × d')).toBeInTheDocument()

    // 先删空再输入：删空的那一刻比例不能丢。
    const spacing = side('做多侧').getByLabelText('网格间距 d')
    await userEvent.clear(spacing)
    await userEvent.type(spacing, '400')

    expect(side('做多侧').getByLabelText('网格价差 g')).toHaveValue('800')
    await saveAndPreview()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { grid_spacing: '400', grid_spread: '800' },
    })
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
    expect(screen.getByText('中性网格不设主仓')).toBeInTheDocument()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      instrument: 'BTC',
      long: { price_low: '75000', core_ratio: '0', batch_count: null },
      short: { price_low: '96000', core_ratio: '0', batch_count: null },
    })

    // 预览里的格子阶梯两侧各一格，按建仓价从高到低：空头侧 91 在上，多头侧 89 在下。
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(3)
    expect(rows[1]).toHaveTextContent('空0')
    expect(rows[1]).toHaveTextContent('91')
    expect(rows[2]).toHaveTextContent('多0')
    expect(rows[2]).toHaveTextContent('89')
  })

  it('b37_填了观点失效价时，滑点上限的百分比换成比率提交', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.type(side('做多侧').getByLabelText('观点失效价'), '70000')
    await userEvent.type(side('做多侧').getByLabelText('失效后剩余数量 R'), '0.3')
    await userEvent.clear(side('做多侧').getByLabelText('失效平仓滑点上限（%）'))
    await userEvent.type(side('做多侧').getByLabelText('失效平仓滑点上限（%）'), '2')
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { invalidation: { price: '70000', remaining: '0.3', max_slippage: '0.02' } },
    })
  })

  it('b37_失效价留空就是不启用：提交 null，滑点写错了也不拦', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.clear(side('做多侧').getByLabelText('失效平仓滑点上限（%）'))
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { invalidation: null },
    })
  })

  it('b40_改成中性网格后，剩余数量和主仓比例显示成要提交的 0', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': { body: neutralPreview() },
    })
    renderEditor(server)

    await userEvent.clear(side('做多侧').getByLabelText('主仓比例 r'))
    await userEvent.type(side('做多侧').getByLabelText('主仓比例 r'), '0.5')
    await userEvent.type(side('做多侧').getByLabelText('观点失效价'), '70000')
    await userEvent.type(side('做多侧').getByLabelText('失效后剩余数量 R'), '0.3')
    await userEvent.click(screen.getByLabelText('启用做空侧'))

    expect(side('做多侧').getByLabelText('失效后剩余数量 R')).toHaveValue('0')
    expect(side('做多侧').getByLabelText('主仓比例 r')).toHaveValue('0')
    await saveAndPreview()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { core_ratio: '0', invalidation: { price: '70000', remaining: '0' } },
    })
  })

  it('第三方市场的标的照原样提交，例如黄金 xyz:GOLD', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.clear(screen.getByLabelText('标的'))
    await userEvent.type(screen.getByLabelText('标的'), 'xyz:GOLD')
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({ instrument: 'xyz:GOLD' })
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

    expect(await screen.findByText(/重新预览/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeDisabled()
  })

  it('b46_标的有别的策略在跑时说清下一步，而不是让人重新预览', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/start': {
        status: 409,
        body: {
          kind: 'instrument_busy',
          error: '标的 BTC 已有活动策略 s-2',
        },
      },
    })
    renderEditor(server)

    await saveAndPreview()
    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    expect(await screen.findByText(/已有活动策略，先停止它/)).toBeInTheDocument()
    // 预览没作废：停掉那一个之后还能用这次预览启动。
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()
  })

  it('服务端正忙时说稍后再点，也不作废预览', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/start': {
        status: 409,
        body: { kind: 'busy', error: '正在执行上一步，稍后再试' },
      },
    })
    renderEditor(server)

    await saveAndPreview()
    await userEvent.click(screen.getByRole('button', { name: '确认启动' }))

    expect(await screen.findByText(/正在执行上一步，稍后再试/)).toBeInTheDocument()
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

    expect(screen.getByText(/已有仓位，启动时按接管规则/)).toBeInTheDocument()
    const planned = screen.getByRole('table', { name: '启动后第一步挂出的订单' })
    // 限价由服务端算好（已避开自己的卖单），页面原样显示。
    expect(within(planned).getByText('88999')).toBeInTheDocument()
    expect(within(planned).getByText('0.091')).toBeInTheDocument()
    const foreign = screen.getByRole('table', { name: '交易所上已有的其他挂单（本系统不撤销）' })
    expect(within(foreign).getByText('777（手动下单）')).toBeInTheDocument()
  })

  it('b34_预览显示满仓压力测试，超过提醒阈值时给出提醒但仍可确认', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': {
        body: preview({
          stress: [
            {
              direction: 'long',
              adverse_price: '75000',
              position: '1',
              average_entry: '86900',
              margin_ratio: '0.36057692',
              liquidation: { kind: 'at', price: '67747.6' },
            },
          ],
          warnings: ['多头侧满仓 1、价格到 75000 时，预计账户保证金率 36.05%，超过提醒阈值 30.00%。'],
        }),
      },
    })
    renderEditor(server)

    await saveAndPreview()

    const table = screen.getByRole('table', { name: '满仓压力测试' })
    expect(within(table).getByText('36.057692%')).toBeInTheDocument()
    expect(within(table).getByText('67747.6')).toBeInTheDocument()
    expect(screen.getByText(/多头侧满仓/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '确认启动' })).toBeEnabled()
  })

  it('b34_强平价远到算不出来时说超出计算范围，不说不会强平', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': {
        body: preview({
          stress: [
            {
              direction: 'short',
              adverse_price: '95000',
              position: '0.0001',
              average_entry: '85000',
              margin_ratio: '0.00001',
              liquidation: { kind: 'out_of_range' },
            },
          ],
        }),
      },
    })
    renderEditor(server)

    await saveAndPreview()

    const table = screen.getByRole('table', { name: '满仓压力测试' })
    expect(within(table).getByText('超出计算范围')).toBeInTheDocument()
    expect(within(table).queryByText('不会强平')).not.toBeInTheDocument()
  })

  it('参数校验不通过时只显示原因，不给确认启动的机会', async () => {
    const server = editorRoutes({
      'POST /api/drafts/s-1/preview': {
        body: preview({
          validation: { valid: false, reason: '做多侧：网格价差 g 10 扣掉两边的 taker 手续费后不赚钱，至少要 12' },
          token: null,
        }),
      },
    })
    renderEditor(server)

    await userEvent.click(screen.getByRole('button', { name: '保存并预览' }))

    // 标题说不通过，正文是服务端给的原因，原样显示。
    const alert = await screen.findByRole('alert')
    expect(within(alert).getByText('校验不通过')).toBeInTheDocument()
    expect(
      within(alert).getByText('做多侧：网格价差 g 10 扣掉两边的 taker 手续费后不赚钱，至少要 12'),
    ).toBeInTheDocument()
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
