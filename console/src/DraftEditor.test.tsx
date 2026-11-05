// 新建、预览、确认启动（B42、B43、B47）。
//
// 页面只发请求、只显示服务端给的数据：数量、批次和格子的划分都在服务端算好。

import { describe, expect, it, vi } from 'vitest'
import { fireEvent, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { DraftEditor } from './DraftEditor'
import { fakeServer, type FakeServer, type Routes } from './test/fakeServer'
import { longSide, neutralPreview, preview, sizingPreview } from './test/fixtures'
import { renderWithQuery } from './test/render'

function editorRoutes(extra: Routes = {}) {
  return fakeServer({
    'POST /api/drafts': { body: { draft_id: 's-1' } },
    'PUT /api/drafts/s-1': { body: { version: 2 } },
    'POST /api/drafts/s-1/preview': { body: preview() },
    'POST /api/drafts/s-1/start': { body: { strategy_id: 's-1' } },
    'POST /api/sizing': { body: sizingPreview() },
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

  it('主仓批数下面显示批次价格范围和段宽，改 L 时跟着变', async () => {
    renderEditor(editorRoutes())
    const long = side('做多侧')
    for (const [label, value] of [
      ['价格下限 L', '75000'],
      ['价格上限 H', '84000'],
      ['主仓比例 r', '0.5'],
      ['主仓批数 n', '5'],
    ]) {
      const input = long.getByLabelText(label)
      await userEvent.clear(input)
      await userEvent.type(input, value)
    }

    // 做多从 H 往下排：最低一批在 L 上方一段，L 是格子的下限。
    expect(long.getByText('主仓 84000 → 76800，段宽 1800')).toBeInTheDocument()

    const low = long.getByLabelText('价格下限 L')
    await userEvent.clear(low)
    await userEvent.type(low, '72750')
    expect(long.getByText('主仓 84000 → 75000，段宽 2250')).toBeInTheDocument()
  })

  it('主仓倾斜下面显示首批和末批的主仓数量，提交时带上倾斜比；留空提交 null', async () => {
    const server = editorRoutes()
    renderEditor(server)
    const long = side('做多侧')
    expect(long.getByText('留空：M 平均分给各批，网格随批')).toBeInTheDocument()
    for (const [label, value] of [
      ['主仓比例 r', '0.7'],
      ['主仓批数 n', '4'],
      ['主仓倾斜（末批÷首批）', '3'],
    ]) {
      const input = long.getByLabelText(label)
      await userEvent.clear(input)
      await userEvent.type(input, value)
    }

    // 主仓 .7 按 3 : 5 : 7 : 9 分：首批 .0875，末批 .2625；网格不随批。
    expect(long.getByText('主仓每批 0.0875 → 0.2625，网格均分给全部格子')).toBeInTheDocument()

    await saveAndPreview()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { core_ratio: '0.7', batch_count: 4, core_skew: '3' },
    })

    await userEvent.clear(long.getByLabelText('主仓倾斜（末批÷首批）'))
    await userEvent.click(screen.getByRole('button', { name: '保存并预览' }))
    await waitFor(() => {
      expect(server.lastBody('PUT', '/api/drafts/s-1')).toMatchObject({ long: { core_skew: null } })
    })
  })

  it('做空侧的批次从 L 往上排；没有主仓时提示留空', async () => {
    renderEditor(editorRoutes())
    await userEvent.click(screen.getByRole('switch', { name: '启用做多侧' }))
    await userEvent.click(screen.getByRole('switch', { name: '启用做空侧' }))
    const short = side('做空侧')
    expect(short.getByText('r = 0 时留空')).toBeInTheDocument()

    for (const [label, value] of [
      ['主仓比例 r', '0.5'],
      ['主仓批数 n', '3'],
    ]) {
      const input = short.getByLabelText(label)
      await userEvent.clear(input)
      await userEvent.type(input, value)
    }

    // 默认空头侧范围 96000–99000：段宽 1000，批次 96000、97000、98000。
    expect(short.getByText('主仓 96000 → 98000，段宽 1000')).toBeInTheDocument()
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
    await userEvent.type(side('做多侧').getByLabelText('失效后保留主仓（%）'), '30')
    await userEvent.clear(side('做多侧').getByLabelText('失效平仓滑点上限（%）'))
    await userEvent.type(side('做多侧').getByLabelText('失效平仓滑点上限（%）'), '2')
    await saveAndPreview()

    // 保留主仓的百分比换成比例提交，R 由服务端按主仓总量算出。
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { invalidation: { price: '70000', remaining: '0', remaining_ratio: '0.3', max_slippage: '0.02' } },
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
    await userEvent.type(side('做多侧').getByLabelText('失效后保留主仓（%）'), '30')
    await userEvent.click(screen.getByLabelText('启用做空侧'))

    expect(side('做多侧').getByLabelText('失效后保留主仓（%）')).toHaveValue('0')
    expect(side('做多侧').getByLabelText('主仓比例 r')).toHaveValue('0')
    await saveAndPreview()
    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { core_ratio: '0', invalidation: { price: '70000', remaining: '0', remaining_ratio: '0' } },
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

describe('按资金、按格数设置（第 2.2 节）', () => {
  it('按资金：提交投入资金，M 占位 0；预览里列出算出的 M、满仓总价值和保证金占用', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.click(side('做多侧').getByRole('radio', { name: '按资金' }))
    expect(side('做多侧').queryByLabelText('最大仓位 M')).not.toBeInTheDocument()
    await userEvent.type(side('做多侧').getByLabelText('投入资金（USDC）'), '85500')
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { capital: '85500', max_position: '0', grid_count: null },
    })
    const sizing = within(screen.getByRole('table', { name: '仓位与网格' }))
    const row = sizing.getAllByRole('row')[1]
    expect(row).toHaveTextContent('20 格，d 1000，g 1000')
    expect(row).toHaveTextContent('85500')
    expect(row).toHaveTextContent('8550')
  })

  it('按格数：提交格数和价差倍数，间距和价差占位 0；倍数沿用按间距时的 g ÷ d', async () => {
    const server = editorRoutes()
    renderEditor(server)

    const spread = side('做多侧').getByLabelText('网格价差 g')
    await userEvent.clear(spread)
    await userEvent.type(spread, '2000')
    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    expect(side('做多侧').queryByLabelText('网格间距 d')).not.toBeInTheDocument()
    expect(side('做多侧').getByLabelText('价差倍数 g÷d')).toHaveValue('2')
    await userEvent.type(side('做多侧').getByLabelText('格数 N'), '20')
    expect(side('做多侧').getByText('间距约 1000')).toBeInTheDocument()
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({
      long: { grid_count: 20, grid_spread_ratio: '2', grid_spacing: '0', grid_spread: '0', capital: null },
    })
  })

  it('按格数预览时价格条按算出的间距画格子，不是表单里占位的 0', async () => {
    const server = editorRoutes()
    const { container } = renderWithQuery(<DraftEditor draftId={null} onBack={() => {}} onStarted={() => {}} />)

    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    await userEvent.type(side('做多侧').getByLabelText('格数 N'), '20')
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({ long: { grid_spacing: '0' } })
    const drawn = [...container.querySelectorAll<HTMLElement>('[data-held]')]
    expect(drawn.length).toBeGreaterThan(0)
    // 每格的宽度是间距在整条上的占比：照占位的 0 画就是 0%。
    for (const slot of drawn) {
      const share = Number(/([\d.]+)%/.exec(slot.style.width)?.[1])
      expect(share).toBeGreaterThan(1)
    }
  })

  it('只积累主仓（r = 1）时格数留空也能提交', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.clear(side('做多侧').getByLabelText('主仓比例 r'))
    await userEvent.type(side('做多侧').getByLabelText('主仓比例 r'), '1')
    await userEvent.type(side('做多侧').getByLabelText('主仓批数 n'), '4')
    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    await saveAndPreview()

    expect(server.lastBody('POST', '/api/drafts')).toMatchObject({ long: { core_ratio: '1', grid_count: null } })
  })

  it('格数不是正整数时不提交', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    await userEvent.type(side('做多侧').getByLabelText('格数 N'), '2.5')
    await userEvent.click(screen.getByRole('button', { name: '保存并预览' }))

    expect(await screen.findByRole('alert')).toHaveTextContent('做多侧：格数 N 填正整数。')
    expect(server.count('POST', '/api/drafts')).toBe(0)
  })

  it('重新打开按资金、按格数存的草稿：还是按资金、按格数，填的值都在', async () => {
    fakeServer({
      'GET /api/drafts/s-1': {
        body: {
          draft_id: 's-1',
          config: {
            instrument: 'BTC',
            long: {
              ...longSide,
              max_position: '0',
              capital: '85500',
              grid_spacing: '0',
              grid_spread: '0',
              grid_count: 20,
              grid_spread_ratio: '1.5',
              invalidation: { price: '70000', remaining: '0', remaining_ratio: '0.25', max_slippage: '0.01' },
            },
            short: null,
          },
          created_at: '2026-11-05T00:00:00Z',
          updated_at: '2026-11-05T00:00:00Z',
        },
      },
    })
    renderWithQuery(<DraftEditor draftId="s-1" onBack={() => {}} onStarted={() => {}} />)

    expect(await side('做多侧').findByDisplayValue('85500')).toBeInTheDocument()
    expect(side('做多侧').getByRole('radio', { name: '按资金' })).toBeChecked()
    expect(side('做多侧').getByRole('radio', { name: '按格数' })).toBeChecked()
    expect(side('做多侧').getByLabelText('格数 N')).toHaveValue('20')
    expect(side('做多侧').getByLabelText('价差倍数 g÷d')).toHaveValue('1.5')
    expect(side('做多侧').getByLabelText('失效后保留主仓（%）')).toHaveValue('25')
  })
})

describe('边填边看仓位划分（第 2.4 节第 1 步）', () => {
  function breakdown() {
    return within(screen.getByRole('region', { name: '做多侧仓位划分' }))
  }

  /** 切到「持仓累计」看法：默认是挂单排布。 */
  async function showHolding() {
    await userEvent.click(await breakdown().findByRole('radio', { name: '持仓累计' }))
  }

  /** 让图知道自己在页面上多大：jsdom 不排版，鼠标位置换不成图上的坐标。 */
  function sized(element: Element, width: number, height: number) {
    vi.spyOn(element, 'getBoundingClientRect').mockReturnValue({
      left: 0,
      width,
      top: 0,
      height,
      right: width,
      bottom: height,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    })
  }

  /** 按资金填：投入 86900、主仓 .7 分 4 批、按格数 20 格、失效价 72000 保留一半。 */
  async function fillByCapital() {
    await userEvent.click(side('做多侧').getByRole('radio', { name: '按资金' }))
    await userEvent.type(side('做多侧').getByLabelText('投入资金（USDC）'), '86900')
    await userEvent.clear(side('做多侧').getByLabelText('主仓比例 r'))
    await userEvent.type(side('做多侧').getByLabelText('主仓比例 r'), '0.7')
    await userEvent.type(side('做多侧').getByLabelText('主仓批数 n'), '4')
    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    await userEvent.type(side('做多侧').getByLabelText('格数 N'), '20')
    await userEvent.type(side('做多侧').getByLabelText('观点失效价'), '72000')
    await userEvent.type(side('做多侧').getByLabelText('失效后保留主仓（%）'), '50')
  }

  it('不保存就算出 M、资金拆分和现价时的持仓', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await fillByCapital()

    await waitFor(() =>
      expect(server.lastBody('POST', '/api/sizing')).toMatchObject({
        long: { capital: '86900', grid_count: 20, core_ratio: '0.7', batch_count: 4 },
      }),
    )
    expect(await breakdown().findByText('最大仓位 M')).toBeInTheDocument()
    expect(breakdown().getByText('88000', { selector: 'span' }).parentElement).toHaveTextContent('现价 88000')
    expect(breakdown().getByText('用到 / 投入').nextElementSibling).toHaveTextContent('86900 / 86900')
    expect(breakdown().getByText('保证金 · 10x').nextElementSibling).toHaveTextContent('8690')
    const split = within(breakdown().getByRole('list', { name: '资金拆分' }))
    expect(split.getByText('主仓 · 失效后保留').parentElement).toHaveTextContent('30625.00 35.2%')
    expect(split.getByText('主仓 · 失效时卖出').parentElement).toHaveTextContent('30625.00 35.2%')
    expect(split.getByText('网格 · 20 格').parentElement).toHaveTextContent('25650.00 29.5%')
    expect(split.queryByText('未用')).not.toBeInTheDocument()
    // 现价 88000：95000、90000 两批主仓和 95000–88000 八格启动就成交，按现价约投入。
    await showHolding()
    expect(breakdown().getByText(/启动即持有/)).toHaveTextContent(
      '现价 启动即持有 0.470 = 主仓 2 批 0.350 + 网格 8 格 0.120，约投入 41360.00',
    )
    expect(server.count('POST', '/api/drafts')).toBe(0)
  })

  it('默认看挂单排布：网格多细一眼看出，每张单画在自己的价格上', async () => {
    const server = editorRoutes()
    renderEditor(server)
    await fillByCapital()

    expect(await breakdown().findByText('网格')).toBeInTheDocument()
    const reading = (label: string) => breakdown().getByText(label).parentElement
    expect(reading('网格')).toHaveTextContent('20 格价差 g 1000')
    // 间距 1000 在 75000–95000 的中间价 85000 上约 1.2%。
    expect(reading('间距 d')).toHaveTextContent('1000≈ 1.2%')
    expect(reading('每格')).toHaveTextContent('0.015≈ 1140–\u200b1425 USDC')
    expect(reading('每轮扣费后')).toHaveTextContent('+14.57–\u200b14.66')

    const layout = breakdown().getByRole('img', { name: '做多侧挂单排布' })
    const order = (name: string) => layout.querySelector(`[data-order="${name}"]`)
    expect(layout.querySelectorAll('[data-order^="batch-"]')).toHaveLength(4)
    expect(layout.querySelectorAll('[data-order^="cell-"]')).toHaveLength(20)
    expect(layout.querySelectorAll('circle')).toHaveLength(20)
    // 现价 88000：前两批和前八格启动就成交，画亮。
    expect(order('batch-1')).toHaveAttribute('data-started', 'true')
    expect(order('batch-2')).toHaveAttribute('data-started', 'false')
    expect(order('cell-7')).toHaveAttribute('data-started', 'true')
    expect(order('cell-8')).toHaveAttribute('data-started', 'false')
  })

  it('鼠标移到某一格或某一批上，写出它的价格、数量和每轮利润', async () => {
    const server = editorRoutes()
    renderEditor(server)
    await fillByCapital()
    const layout = await breakdown().findByRole('img', { name: '做多侧挂单排布' })
    // 主仓一排在 0–48，网格一排在 70–118，整张图高 136。
    sized(layout, 360, 136)
    const at = (price: number) => ((price - 70080) / (96720 - 70080)) * 360

    fireEvent.mouseMove(layout, { clientX: at(91500), clientY: 100 })
    expect(breakdown().getByText(/每轮/, { selector: 'p' })).toHaveTextContent(
      '格 3（批 0） · 买 92000 → 卖 93000 · 0.015 · 每轮 +14.58 · 启动即成交',
    )

    fireEvent.mouseMove(layout, { clientX: at(85000), clientY: 30 })
    expect(breakdown().getByText(/主仓批/)).toHaveTextContent('主仓批 2 · 买 85000 · 0.175 · 约 14875.00 USDC')

    fireEvent.mouseLeave(layout)
    expect(breakdown().getByText('移到某一张单上看它的价格、数量和每轮利润。')).toBeInTheDocument()
  })

  it('网格粗时最外一格的平仓价也画在图里，停在空心点上选中的是这一格', async () => {
    // 10 格、间距和价差都是 2000：第 0 格 95000 买、97000 卖，在范围外面。
    const fixture = sizingPreview()
    const coarse = Array.from({ length: 10 }, (_, index) => ({
      ...fixture.cells[0],
      index,
      batch: null,
      entry_price: String(95000 - index * 2000),
      exit_price: String(97000 - index * 2000),
    }))
    const server = editorRoutes({
      'POST /api/sizing': {
        body: sizingPreview({
          batches: [],
          cells: coarse,
          immediate_fills: [],
          sizing: [{ ...fixture.sizing[0], grid_spacing: '2000', grid_spread: '2000', cell_count: 10 }],
        }),
      },
    })
    renderEditor(server)
    const layout = await breakdown().findByRole('img', { name: '做多侧挂单排布' })

    const dots = [...layout.querySelectorAll('circle')].map((dot) => Number(dot.getAttribute('cx')))
    expect(dots).toHaveLength(10)
    for (const cx of dots) expect(cx).toBeGreaterThan(0)
    for (const cx of dots) expect(cx).toBeLessThan(360)

    // 只有网格一排：在 0–48，空心点那一行在 y = 18，整张图高 66。
    sized(layout, 360, 66)
    fireEvent.mouseMove(layout, { clientX: dots[3], clientY: 18 })
    expect(breakdown().getByText(/每轮/, { selector: 'p' })).toHaveTextContent('格 3 · 买 89000 → 卖 91000')
  })

  it('只积累主仓时说没有网格，图上只有主仓一排', async () => {
    const server = editorRoutes({ 'POST /api/sizing': { body: sizingPreview({ cells: [] }) } })
    renderEditor(server)

    expect(await breakdown().findByText('只积累主仓，没有网格。')).toBeInTheDocument()
    const layout = breakdown().getByRole('img', { name: '做多侧挂单排布' })
    expect(layout.querySelectorAll('[data-order^="cell-"]')).toHaveLength(0)
    expect(layout.querySelectorAll('[data-order^="batch-"]')).toHaveLength(4)
  })

  it('投入没用满时把零头标成未用', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await fillByCapital()
    await userEvent.clear(side('做多侧').getByLabelText('投入资金（USDC）'))
    await userEvent.type(side('做多侧').getByLabelText('投入资金（USDC）'), '90000')

    const split = within(await breakdown().findByRole('list', { name: '资金拆分' }))
    await waitFor(() => expect(split.getByText('未用').parentElement).toHaveTextContent('3100.00 3.4%'))
  })

  it('鼠标移到阶梯上看那个价位持有多少，越过失效价只剩 R', async () => {
    const server = editorRoutes()
    renderEditor(server)
    await fillByCapital()
    await showHolding()
    const ladder = await breakdown().findByRole('img', { name: '做多侧持仓阶梯' })
    sized(ladder, 360, 116)
    // 横轴从 70080 到 96720：失效价 72000 外留 8%，最外一格的平仓价 96000 外留 3%。
    const at = (price: number) => ((price - 70080) / (96720 - 70080)) * 360

    fireEvent.mouseMove(ladder, { clientX: at(82400) })
    expect(breakdown().getByText(/一路跌到/)).toHaveTextContent(
      '一路跌到 82400 持有 0.720 = 主仓 3 批 0.525 + 网格 13 格 0.195',
    )

    fireEvent.mouseMove(ladder, { clientX: at(71000) })
    expect(breakdown().getByText(/越过失效价/)).toHaveTextContent('越过失效价 72000：主仓减到 0.35，网格全平，策略停止')

    fireEvent.mouseLeave(ladder)
    expect(breakdown().getByText(/启动即持有/)).toBeInTheDocument()
  })

  it('没有主仓时只说网格，拆分条只有网格一段', async () => {
    const fixture = sizingPreview()
    const server = editorRoutes({
      'POST /api/sizing': {
        body: sizingPreview({
          batches: [],
          immediate_fills: fixture.immediate_fills.filter((fill) => 'cell' in fill.unit),
          sizing: [
            { ...fixture.sizing[0], core_quantity: '0', core_notional: '0', notional: '25650', invalidation_remaining: null },
          ],
        }),
      },
    })
    renderEditor(server)
    await showHolding()

    expect(await breakdown().findByText(/启动即持有/)).toHaveTextContent('现价 启动即持有 0.120（网格 8 格），约投入 10560.00')
    const split = within(breakdown().getByRole('list', { name: '资金拆分' }))
    expect(split.getAllByRole('listitem')).toHaveLength(1)
    expect(split.getByText('网格 · 20 格').parentElement).toHaveTextContent('25650.00 100.0%')
  })

  it('设了限速时启动只算放行的那一批，不把现价以上的都算成持有', async () => {
    const fixture = sizingPreview()
    const server = editorRoutes({
      'POST /api/sizing': { body: sizingPreview({ immediate_fills: fixture.immediate_fills.slice(0, 1) }) },
    })
    renderEditor(server)
    await showHolding()

    expect(await breakdown().findByText(/启动即持有/)).toHaveTextContent(
      '现价 启动即持有 0.175 = 主仓 1 批 0.175 + 网格 0 格 0.000，约投入 15400.00',
    )
  })

  it('读不到杠杆时其余照常，保证金一格写明读不到', async () => {
    const server = editorRoutes({ 'POST /api/sizing': { body: sizingPreview({ leverage: null }) } })
    renderEditor(server)

    expect(await breakdown().findByText('读不到杠杆')).toBeInTheDocument()
    expect(breakdown().getByText('最大仓位 M').nextElementSibling).toHaveTextContent('1')
  })

  it('改了参数、新结果回来之前，旧数字压暗并写更新中', async () => {
    let answer: (value: unknown) => void = () => {}
    const pending = new Promise((resolve) => {
      answer = resolve
    })
    let calls = 0
    const server = editorRoutes({
      'POST /api/sizing': () => {
        calls += 1
        return calls === 1 ? { body: sizingPreview() } : { body: sizingPreview(), wait: pending }
      },
    })
    renderEditor(server)
    await breakdown().findByText('最大仓位 M')

    await userEvent.type(side('做多侧').getByLabelText('观点失效价'), '72000')

    const region = screen.getByRole('region', { name: '做多侧仓位划分' })
    await waitFor(() => expect(region).toHaveAttribute('aria-busy', 'true'))
    expect(within(region).getByText('更新中…')).toBeInTheDocument()
    answer(null)
    await waitFor(() => expect(region).toHaveAttribute('aria-busy', 'false'))
  })

  it('同时涉及两侧的原因两侧都显示', async () => {
    const reason = '做多侧上限 H 95000 须低于做空侧下限 L 91000'
    const server = editorRoutes({
      'POST /api/sizing': {
        body: sizingPreview({ validation: { valid: false, reason }, sizing: [], batches: [], cells: [] }),
      },
    })
    renderEditor(server)
    await userEvent.click(screen.getByRole('switch', { name: '启用做空侧' }))

    const shortSide = within(screen.getByRole('region', { name: '做空侧仓位划分' }))
    expect(await shortSide.findByText(reason)).toBeInTheDocument()
    expect(breakdown().getByText(reason)).toBeInTheDocument()
  })

  it('参数不成立时在划分的位置说明原因', async () => {
    const reason = '做多侧：第 1 批（90000）主仓只有 0，不够最低下单金额（至少要 0.001）；减少批数 n'
    const server = editorRoutes({
      'POST /api/sizing': {
        body: sizingPreview({ validation: { valid: false, reason }, sizing: [], batches: [], cells: [] }),
      },
    })
    renderEditor(server)

    expect(await breakdown().findByText('还算不出划分')).toBeInTheDocument()
    expect(breakdown().getByText(reason, { exact: false })).toBeInTheDocument()
  })

  it('数字没填完时请它先填完，不报解析错误', async () => {
    const server = editorRoutes({ 'POST /api/sizing': { status: 422, body: { error: 'invalid number' } } })
    renderEditor(server)

    await userEvent.clear(side('做多侧').getByLabelText('价格下限 L'))

    expect(await breakdown().findByText('填完各项数字后显示划分。')).toBeInTheDocument()
    expect(breakdown().queryByText(/invalid number/)).not.toBeInTheDocument()
  })

  it('表单本身表达不出来的参数，直接在划分里提示', async () => {
    const server = editorRoutes()
    renderEditor(server)

    await userEvent.click(side('做多侧').getByRole('radio', { name: '按格数' }))
    await userEvent.type(side('做多侧').getByLabelText('格数 N'), '2.5')

    expect(await breakdown().findByText('做多侧：格数 N 填正整数。', { exact: false })).toBeInTheDocument()
  })
})
