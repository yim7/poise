// 运行详情：批次表、格子表、挂单、等待原因、参数版本，以及暂停、恢复、停止（B45）。

import { describe, expect, it, vi } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { StrategyDetailView } from './StrategyDetail'
import { fakeServer, type Routes } from './test/fakeServer'
import { config, detail, longSide, neutralDetail, shortDetail, summary } from './test/fixtures'
import { renderWithQuery } from './test/render'

function detailRoutes(extra: Routes = {}) {
  return fakeServer({
    'GET /api/strategies/s-1': { body: detail() },
    'POST /api/strategies/s-1/pause': { body: { canceled: ['0x1', '0x2'], status: 'paused' } },
    'POST /api/strategies/s-1/resume': {
      body: { Resumed: { strategy_id: 's-1', status: 'running', placed: ['0x3'], canceled: [], applied_fills: 0, position: '0.015', expected_position: '0.015' } },
    },
    'POST /api/strategies/s-1/stop': { body: { canceled: ['0x1'], status: 'stopped' } },
    'PUT /api/strategies/s-1/config': { body: { version: 3 } },
    ...extra,
  })
}

describe('运行详情', () => {
  it('显示批次表、格子表、挂单、等待原因和参数版本', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('主仓批次表')).toBeInTheDocument()
    expect(screen.getByText('网格格子表')).toBeInTheDocument()
    expect(screen.getByText('在场挂单')).toBeInTheDocument()
    // 「等待所属批次」既是格子状态也是等待原因，两处都该出现。
    expect(screen.getAllByText('等待所属批次')).toHaveLength(2)
    expect(screen.getByText('价格未到')).toBeInTheDocument()
    // 「参数版本」既是事实项，也是版本表的标题。
    expect(screen.getAllByText('参数版本')).toHaveLength(2)
    expect(screen.getByText('做多第 0 批主仓')).toBeInTheDocument()
    // 仓位和应有持仓一致时不提示。
    expect(screen.queryByText(/仓位与应有持仓不一致/)).not.toBeInTheDocument()
  })

  it('b35_显示账户保证金率；读不到时如实说读不到', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('账户保证金率')).toBeInTheDocument()
    // 服务端给的是比率 0.0123，页面只换成百分比写法。
    expect(screen.getByText('1.23%')).toBeInTheDocument()
  })

  it('b36_保证金率读不到时显示读不到和对应的等待原因', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ margin_ratio: null, waiting: ['margin_ratio_unavailable'] }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('读不到')).toBeInTheDocument()
    expect(screen.getByText('保证金率读不到')).toBeInTheDocument()
  })

  it('b45_停止要二次确认，确认后调用停止接口并提示仓位保留', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    expect(server.count('POST', '/api/strategies/s-1/stop')).toBe(0)

    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))

    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/stop')).toBe(1)
    })
    expect(await screen.findByText(/仓位保留/)).toBeInTheDocument()
  })

  it('b45_暂停还没撤完时按接口返回的状态说正在撤单', async () => {
    // 撤单回执丢了、成交还没对完账的时候，接口回的是「暂停中」：订单还在交易所上，
    // 后台每一轮继续撤。这时候说「已暂停」，用户以为挂单已经撤掉了。
    detailRoutes({
      'POST /api/strategies/s-1/pause': {
        body: { canceled: ['0x1', '0x2'], status: 'pausing' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '暂停' }))

    expect(await screen.findByText(/正在撤单/)).toBeInTheDocument()
    expect(screen.queryByText(/已暂停/)).not.toBeInTheDocument()
  })

  it('b45_暂停确认收尾之后才说已暂停', async () => {
    detailRoutes({
      'POST /api/strategies/s-1/pause': {
        body: { canceled: ['0x1', '0x2'], status: 'paused' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '暂停' }))

    expect(await screen.findByText('已暂停，撤销 2 张订单。')).toBeInTheDocument()
  })

  it('b45_停止还没撤完时按接口返回的状态说正在撤单，不说已归档', async () => {
    detailRoutes({
      'POST /api/strategies/s-1/stop': {
        body: { canceled: ['0x1'], status: 'stopping' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))

    expect(await screen.findByText(/正在撤单/)).toBeInTheDocument()
    expect(screen.queryByText(/已停止并归档/)).not.toBeInTheDocument()
    // 仓位照旧保留，说清楚。
    expect(screen.getByText(/仓位保留/)).toBeInTheDocument()
  })

  it('b19_恢复还没收尾时说明还有遗留订单没收场', async () => {
    // 恢复也先撤遗留订单：收不了场就留在「恢复中」，一张新单都不下。
    // 收不了场不等于没撤掉——结果没查出来的订单也在里面，所以不说「会继续撤」。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ summary: summary({ status: 'restoring' }) }),
      },
      'POST /api/strategies/s-1/resume': { body: { WaitingForSettle: { working: 3 } } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '恢复' }))

    expect(await screen.findByText(/3 张遗留订单没收场/)).toBeInTheDocument()
  })

  it('b45_已归档的策略只读：没有暂停、恢复、停止，也没有改参数的入口', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'stopped', working_orders: 0 }),
          orders: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('策略已归档，只能只读查看。')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '停止' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
    expect(screen.getByText('没有在场挂单。')).toBeInTheDocument()
    // 不在运行：没有在等的事，也没有「应有持仓」可比。
    expect(screen.getByText('策略已归档，没有在等的事。')).toBeInTheDocument()
    expect(screen.queryByText('应有持仓')).not.toBeInTheDocument()
  })

  it('b37_失效中只能停止：显示减仓进度，没有暂停、恢复和改参数', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'invalidating' }),
          note: '失效减仓中：标记价越过了失效价，正在用只减仓单减仓，还要减 0.2。',
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/还要减 0.2/)).toBeInTheDocument()
    expect(screen.getByRole('heading', { level: 2 })).toHaveTextContent('失效中')
    expect(screen.getByRole('button', { name: '停止' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
  })

  it('b39d_已失效只读，说明里显示没减掉的尾量', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'invalidated', working_orders: 0 }),
          orders: [],
          note: '已失效：……超出剩余数量的 0.0005 不足最低下单金额，下不了单，由你决定是否手动处理。',
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/0.0005 不足最低下单金额/)).toBeInTheDocument()
    expect(screen.getByText(/新建一个策略接管剩下的仓位/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '停止' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()
  })

  it('不在运行时没有在等的事，也不拿仓位和应有持仓比', async () => {
    // 服务端只在运行中才算应有持仓：不在运行时这一项是 0，拿它比较会一直报不一致。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'paused' }),
          expected_position: '0',
          waiting: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('策略已暂停，没有在等的事。')).toBeInTheDocument()
    expect(screen.queryByText(/仓位与应有持仓不一致/)).not.toBeInTheDocument()
    // 交易所仓位照常显示。
    expect(screen.getByText('交易所仓位')).toBeInTheDocument()
  })

  it('b45_暂停中的策略说明还在撤单，给出原因，并且能改主意恢复', async () => {
    // 暂停分两步：先落「暂停中」不再下新单，撤单和成交对账收尾之后才是「已暂停」。
    // 页面要说出它还在走、为什么还没走完，用户才知道该不该等。
    const server = detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'pausing' }),
          note: '还有 2 张订单没撤掉，正在重试。',
          waiting: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/BTC · 做多 · 暂停中/)).toBeInTheDocument()
    expect(screen.getByText('还有 2 张订单没撤掉，正在重试。')).toBeInTheDocument()
    // 已经在撤单：不再给暂停按钮，但可以恢复，也可以直接停止。
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: '停止' })).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '恢复' }))
    expect(server.count('POST', '/api/strategies/s-1/resume')).toBe(1)
  })

  it('b45_停止中的策略只等后台收尾，没有暂停、恢复和重复停止', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'stopping' }),
          note: '还有 1 张订单没撤掉、1 张订单的成交没对完账。',
          waiting: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/BTC · 做多 · 停止中/)).toBeInTheDocument()
    expect(
      screen.getByText('还有 1 张订单没撤掉、1 张订单的成交没对完账。'),
    ).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '停止' })).not.toBeInTheDocument()
    // 还没归档：仍然看得到在等什么，而不是「没有在等的事」。
    expect(screen.queryByText(/没有在等的事/)).not.toBeInTheDocument()
  })

  it('b19_恢复失败的策略显示原因，可以立刻再试一次', async () => {
    const server = detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'restoring' }),
          note: '恢复失败：rate limited。后台会继续重试。',
          waiting: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/BTC · 做多 · 恢复中/)).toBeInTheDocument()
    expect(screen.getByText('恢复失败：rate limited。后台会继续重试。')).toBeInTheDocument()
    // 恢复中一张新单都没下，也没有在等行情。
    expect(screen.queryByText('价格未到')).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '恢复' }))
    expect(server.count('POST', '/api/strategies/s-1/resume')).toBe(1)
  })

  it('详情标出这份读数是什么时候取的', async () => {
    // 行情、仓位、可用资金来自后台的快照，不是点开这一页时现取的：不标时刻，
    // 一份几分钟前的读数看起来和现在的一样。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ taken_at: '2026-10-15T00:00:01Z' }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const label = await screen.findByText('读数时刻')
    const row = label.closest('div')
    expect(row?.textContent).toContain(
      new Date('2026-10-15T00:00:01Z').toLocaleString('zh-CN', { hour12: false }),
    )
  })

  it('不在运行的策略不给改参数，说明为什么', async () => {
    // 服务端只让运行中的策略改参数：已暂停的改了也不会生效，给个能点的按钮只会
    // 让用户以为改动被接受了。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ summary: summary({ status: 'paused' }) }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/策略已暂停，现在改不了参数/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
  })

  it('暂停中的策略可以恢复，恢复后说明挂了几张单', async () => {
    const server = detailRoutes({
      'GET /api/strategies/s-1': { body: detail({ summary: summary({ status: 'paused' }) }) },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '恢复' }))

    expect(await screen.findByText(/挂上 1 张/)).toBeInTheDocument()
    expect(server.count('POST', '/api/strategies/s-1/resume')).toBe(1)
  })

  it('b44_运行中改限速间隔会提交整份参数，并显示新的版本号', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/第 3 版/)).toBeInTheDocument()
    expect(server.lastBody('PUT', '/api/strategies/s-1/config')).toMatchObject({
      instrument: 'BTC',
      long: { price_low: '75000', throttle_interval: [1800, 0] },
      short: null,
    })
  })

  it('b44_运行中改观点失效设置会提交整份参数，限速间隔保持原样', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧观点失效价'), '70000')
    await userEvent.type(screen.getByLabelText('做多侧失效后剩余数量 R'), '0.3')
    await userEvent.clear(screen.getByLabelText('做多侧失效平仓滑点上限（%）'))
    await userEvent.type(screen.getByLabelText('做多侧失效平仓滑点上限（%）'), '0.5')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/第 3 版/)).toBeInTheDocument()
    expect(server.lastBody('PUT', '/api/strategies/s-1/config')).toMatchObject({
      long: {
        throttle_interval: null,
        invalidation: { price: '70000', remaining: '0.3', max_slippage: '0.005' },
      },
      short: null,
    })
  })

  it('b44_失效滑点上限不是数字时不提交', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧观点失效价'), '70000')
    await userEvent.clear(screen.getByLabelText('做多侧失效平仓滑点上限（%）'))
    await userEvent.type(screen.getByLabelText('做多侧失效平仓滑点上限（%）'), '一点')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/要填百分比数字/)).toBeInTheDocument()
    expect(server.count('PUT', '/api/strategies/s-1/config')).toBe(0)
  })

  it('b44_限速填的不是正数时不提交，也不把它当成不限速', async () => {
    // 把「写错了」当成「不限速」提交上去，用户以为限了速，实际提交的是另一份参数。
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '十分钟')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/要填一个正数/)).toBeInTheDocument()
    expect(server.count('PUT', '/api/strategies/s-1/config')).toBe(0)
  })

  it('b44_没改限速时提交的间隔和原来一模一样', async () => {
    // 间隔不一定是整分钟：显示和提交都不能取整，否则不改这一项也会把它改掉。
    const server = detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({
            config: { ...config, long: { ...longSide, throttle_interval: [90, 0] } },
          }),
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    expect(screen.getByLabelText('做多侧限速间隔 τ（分钟）')).toHaveValue('1.5')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/第 3 版/)).toBeInTheDocument()
    expect(server.lastBody('PUT', '/api/strategies/s-1/config')).toMatchObject({
      long: { throttle_interval: [90, 0] },
    })
  })

  it('b44_保存限速撞上服务端正在走一步时说清楚等一下再点', async () => {
    detailRoutes({
      'PUT /api/strategies/s-1/config': {
        status: 409,
        body: { kind: 'busy', error: 'strategy `s-1` is already being driven' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/稍等一下再点保存/)).toBeInTheDocument()
  })

  it('b44_改了不让改的参数时把服务端的原因显示出来', async () => {
    detailRoutes({
      'PUT /api/strategies/s-1/config': {
        status: 400,
        body: { error: '`grid_spacing` cannot be changed while the strategy is running' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/grid_spacing/)).toBeInTheDocument()
  })

  it('仓位核对失败时提示会被暂停', async () => {
    // 服务端说「仓位核对失败已暂停」才是真的会暂停：这是它给的判定，不是网页自己比出来的。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          expected_position: '0.03',
          waiting: ['position_check_failed'],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/仓位核对失败：策略会被暂停/)).toBeInTheDocument()
  })

  it('仓位对不上但数据还在变时不说会被暂停', async () => {
    // 这种情况服务端的结论是「下一步再核对」，不会暂停；按对不上就报暂停会吓着用户。
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          expected_position: '0.03',
          waiting: ['position_unstable'],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('仓位数据还在变化')).toBeInTheDocument()
    expect(screen.queryByText(/策略会被暂停/)).not.toBeInTheDocument()
  })

  it('做空网格：类型、格子表和限速入口都按空头侧显示', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { body: shortDetail() },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/BTC · 做空 · 运行中/)).toBeInTheDocument()
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(2)
    expect(rows[1]).toHaveTextContent('做空')
    // 空头侧的开空价在买平价之上：表头说的是建仓价/平仓价，不是买价/卖价。
    expect(rows[1]).toHaveTextContent('96000')
    expect(rows[1]).toHaveTextContent('95000')
    // 只配了空头侧：只有一个限速输入。
    expect(screen.getByLabelText('做空侧限速间隔 τ（分钟）')).toBeInTheDocument()
    expect(screen.queryByLabelText('做多侧限速间隔 τ（分钟）')).not.toBeInTheDocument()
  })

  it('中性网格：两侧的格子都在，两侧各有一个限速输入', async () => {
    const server = detailRoutes({
      'GET /api/strategies/s-1': { body: neutralDetail() },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/BTC · 中性 · 运行中/)).toBeInTheDocument()
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(3)
    expect(rows[1]).toHaveTextContent('做多')
    expect(rows[2]).toHaveTextContent('做空')

    await userEvent.type(screen.getByLabelText('做空侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/第 3 版/)).toBeInTheDocument()
    // 提交的是整份参数：没改的那一侧原样带回去，两侧的限速各归各。
    expect(server.lastBody('PUT', '/api/strategies/s-1/config')).toMatchObject({
      long: { price_low: '79', throttle_interval: null },
      short: { price_low: '91', throttle_interval: [1800, 0] },
    })
  })

  it('详情读不到时把原因显示出来', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { status: 404, body: { error: 'strategy `s-1` is not stored' } },
    })
    const onBack = vi.fn()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={onBack} />)

    expect(await screen.findByText(/not stored/)).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '返回列表' }))
    expect(onBack).toHaveBeenCalled()
  })
})
