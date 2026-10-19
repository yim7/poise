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

/** 标题行：大标题是标的，旁边是类型和状态两个标识。 */
async function expectTitle(kind: string, status: string) {
  const heading = await screen.findByRole('heading', { level: 1, name: 'BTC' })
  const row = heading.parentElement as HTMLElement
  expect(within(row).getByText(kind)).toBeInTheDocument()
  expect(within(row).getByText(status)).toBeInTheDocument()
}

describe('运行详情', () => {
  it('显示批次表、格子表、挂单、等待原因和参数版本', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByRole('region', { name: '主仓批次' })).toBeInTheDocument()
    expect(screen.getByRole('region', { name: '网格格子' })).toBeInTheDocument()
    expect(screen.getByRole('region', { name: '在场挂单' })).toBeInTheDocument()
    // 格子还没启用时状态列写「等批次」；等待原因里写全称。
    expect(within(screen.getByRole('region', { name: '网格格子' })).getByText('等批次')).toBeInTheDocument()
    expect(screen.getByText('等待所属批次')).toBeInTheDocument()
    expect(screen.getByText('价格未到')).toBeInTheDocument()
    const versions = screen.getByRole('region', { name: '参数版本' })
    expect(within(versions).getByText('v2')).toBeInTheDocument()
    expect(within(versions).getByText('v1')).toBeInTheDocument()
    expect(within(screen.getByRole('region', { name: '在场挂单' })).getByText('多批0')).toBeInTheDocument()
    // 仓位和应有持仓一致时不显示应有持仓。
    expect(screen.queryByText(/应有持仓/)).not.toBeInTheDocument()
  })

  it('仓位和应有持仓不一致时才显示应有持仓', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { body: detail({ position: '0.03', expected_position: '0.015' }) },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/应有持仓/)).toHaveTextContent('应有持仓 0.015')
  })

  it('格子和挂单用简写：多3、开平、买卖，数量写成已成交 / 总量，状态只在异常时出现', async () => {
    const base = detail()
    const order = (
      client_order_id: string,
      unit: { cell: ['long' | 'short', number] },
      purpose: 'grid_entry' | 'grid_exit',
      limit_price: string,
      cumulative_filled: string,
    ) => ({
      ...base.orders[0],
      record: {
        ...base.orders[0].record,
        client_order_id,
        unit,
        purpose,
        limit_price,
        quantity: '0.02',
        cumulative_filled,
      },
    })
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          cells: base.cells.filter((cell) => cell.status !== 'waiting_for_batch'),
          orders: [
            order('0x1', { cell: ['long', 3] }, 'grid_entry', '93000', '0'),
            order('0x2', { cell: ['short', 1] }, 'grid_exit', '97000', '0.0154'),
          ],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const orders = await screen.findByRole('region', { name: '在场挂单' })
    const rows = within(orders).getAllByRole('row')
    // 按限价从高到低，和格子阶梯一致。
    expect(rows[1]).toHaveTextContent('空1')
    expect(rows[1]).toHaveTextContent('平')
    // 空头侧平仓是买。
    expect(rows[1]).toHaveTextContent('买')
    expect(rows[1]).toHaveTextContent('0.0154 / 0.02')
    expect(rows[2]).toHaveTextContent('多3')
    expect(rows[2]).toHaveTextContent('开')
    expect(rows[2]).toHaveTextContent('买')
    // 全部在场：不出现状态列。
    expect(within(orders).queryByText('在场')).not.toBeInTheDocument()
    expect(within(orders).queryByRole('columnheader', { name: '状态' })).not.toBeInTheDocument()

    // 格子全部已启用：没有状态列，也不写「持有中」「等待买入」。
    const ladder = screen.getByRole('region', { name: '网格格子' })
    expect(within(ladder).getByText('多1')).toBeInTheDocument()
    expect(within(ladder).queryByRole('columnheader', { name: '状态' })).not.toBeInTheDocument()
    expect(within(ladder).queryByText(/持有中|等待买入|等待买回/)).not.toBeInTheDocument()
  })

  it('运行中没有在等的事、也没有提醒时，不显示等待区', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { body: detail({ waiting: [] }) },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await expectTitle('做多', '运行中')
    expect(screen.queryByRole('region', { name: '等待' })).not.toBeInTheDocument()
  })

  it('b35_显示账户保证金率；读不到时如实说读不到', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('账户保证金率')).toBeInTheDocument()
    // 服务端给的是比率 0.0123，页面只换成百分比写法。
    expect(screen.getByText('1.23%')).toBeInTheDocument()
  })

  it('显示资金费率：每小时和年化', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const rate = await screen.findByText(/^0.00125%\/时 · 年化 10.95%$/)
    // 读到的时刻放在悬停提示里。
    expect(rate).toHaveAttribute('title', expect.stringMatching(/^读于 /))
  })

  it('显示累计资金费、网格已实现利润和资金费占的比例', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const readings = await screen.findByRole('region', { name: '关键读数' })
    expect(within(readings).getByText('-1.5')).toBeInTheDocument()
    expect(within(readings).getByText(/^截至 .+/)).toBeInTheDocument()
    expect(within(readings).getByText('网格利润（估）')).toBeInTheDocument()
    expect(within(readings).getByText('-5%')).toBeInTheDocument()
  })

  it('还没有网格利润时不算比例', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ funding_share: null, grid_realized_profit: '0' }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const ratio = await screen.findByText('资金费 / 网格利润')
    expect(ratio.parentElement).toHaveTextContent('资金费 / 网格利润—')
  })

  it('负资金费率显示负号：空头付', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ funding_rate: { hourly: '-0.00027774', annual: '-2.4330532' } }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/^-0.027774%\/时 · 年化 -243.30532%$/)).toBeInTheDocument()
  })

  it('暂停时说明设了失效价的照样会减仓', async () => {
    const paused = summary({ status: 'paused' })
    paused.config = {
      ...paused.config,
      long: { ...longSide, invalidation: { price: '70000', remaining: '0', max_slippage: '0.01' } },
    }
    detailRoutes({ 'GET /api/strategies/s-1': { body: detail({ summary: paused }) } })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/暂停期间越过失效价照样减仓/)).toBeInTheDocument()
  })

  it('运行中也显示说明：保证金率、资金费率、操作次数这些提醒在说明里', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ note: '资金费率不利：年化 87.60%（阈值 50.00%）。' }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/资金费率不利/)).toBeInTheDocument()
    expect(screen.getByText('价格未到')).toBeInTheDocument()
  })

  it('b30_已暂停时显示暂停原因', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'paused' }),
          note: '交易所以操作次数超限拒绝了请求，策略已暂停：减少格子或加大间距，确认额度恢复后再恢复。',
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText(/以操作次数超限拒绝/)).toBeInTheDocument()
  })

  it('b05_提前结束的批次显示少建的主仓和网格', async () => {
    const base = detail()
    const ended = {
      ...base.batches[0],
      finished: true,
      core_shortfall: '0.00001',
      grid_shortfall: '0.00002',
    }
    detailRoutes({ 'GET /api/strategies/s-1': { body: detail({ batches: [ended] }) } })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('0.00001 / 0.00002')).toBeInTheDocument()
    expect(screen.getByText('已结束')).toBeInTheDocument()
  })

  it('b30_显示今日操作次数和每日上限', async () => {
    detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    expect(await screen.findByText('今日操作次数')).toBeInTheDocument()
    expect(screen.getByText('12 / 50000')).toBeInTheDocument()
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

  it('b49_停止确认之后再问是否市价平仓，选了就调用平仓接口', async () => {
    const server = detailRoutes({
      'POST /api/strategies/s-1/close': { body: { status: 'stopping' } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))

    // 第二次确认紧接着弹出，不等撤单收尾：显示要平的净仓位、标记价和最差成交价（标记价
    // 让出 5%）。
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/stop')).toBe(1)
    })
    expect(within(dialog).getByText('0.015')).toBeInTheDocument()
    expect(within(dialog).getByText('95500')).toBeInTheDocument()
    // 多仓卖出：95500 × 0.95。
    expect(within(dialog).getByText('约 90725')).toBeInTheDocument()
    expect(server.count('POST', '/api/strategies/s-1/close')).toBe(0)

    await userEvent.click(within(dialog).getByRole('button', { name: '市价平仓' }))

    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/close')).toBe(1)
    })
    expect(await screen.findByText(/撤单收尾后市价平仓/)).toBeInTheDocument()
  })

  it('b49_停止请求还没回来时就问是否平仓：撤单要好几秒，不让人干等', async () => {
    let finish: () => void = () => {}
    const pending = new Promise<void>((resolve) => {
      finish = resolve
    })
    detailRoutes({
      'POST /api/strategies/s-1/stop': {
        body: { canceled: ['0x1'], status: 'stopping' },
        wait: pending,
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))

    expect(await screen.findByRole('alertdialog', { name: '市价平仓？' })).toBeInTheDocument()
    finish()
  })

  it('b49_停止请求还没回来时就确认平仓：平仓请求等停止成功之后才发', async () => {
    // 服务端一个策略同一时间只做一件事，最多等 3 秒；撤单常常更久。平仓请求要排在停止之后，
    // 不能指望服务端替我们排队。
    let finish: () => void = () => {}
    const pending = new Promise<void>((resolve) => {
      finish = resolve
    })
    const server = detailRoutes({
      'POST /api/strategies/s-1/stop': { body: { canceled: ['0x1'], status: 'stopping' }, wait: pending },
      'POST /api/strategies/s-1/close': { body: { status: 'stopping' } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await userEvent.click(within(dialog).getByRole('button', { name: '市价平仓' }))

    expect(server.count('POST', '/api/strategies/s-1/close')).toBe(0)
    finish()
    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/close')).toBe(1)
    })
    expect(await screen.findByText(/撤单收尾后市价平仓/)).toBeInTheDocument()
  })

  it('停止没成功时不发平仓请求，说清原因', async () => {
    let fail: () => void = () => {}
    const pending = new Promise<void>((resolve) => {
      fail = resolve
    })
    const server = detailRoutes({
      'POST /api/strategies/s-1/stop': {
        status: 409,
        body: { kind: 'busy', error: '正在执行上一步，稍后再试' },
        wait: pending,
      },
      'POST /api/strategies/s-1/close': { body: { status: 'stopping' } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await userEvent.click(within(dialog).getByRole('button', { name: '市价平仓' }))
    fail()

    expect(await screen.findByText(/停止没成功，没有平仓/)).toBeInTheDocument()
    expect(server.count('POST', '/api/strategies/s-1/close')).toBe(0)
  })

  it('停止的响应失败、关掉了第二次确认之后，再点市价平仓照常发出', async () => {
    // 服务端其实执行了停止，但响应丢了：页面刷新后已经是停止中。这时独立发起的平仓不该再
    // 等那次失败的停止。
    let stopped = false
    const server = detailRoutes({
      'GET /api/strategies/s-1': () => ({
        body: detail({ summary: summary({ status: stopped ? 'stopping' : 'running' }) }),
      }),
      'POST /api/strategies/s-1/stop': () => {
        stopped = true
        return { status: 502, body: { kind: 'exchange', error: '交易所暂不可用：timeout' } }
      },
      'POST /api/strategies/s-1/close': { body: { status: 'stopping' } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await userEvent.click(within(dialog).getByRole('button', { name: '保留仓位' }))
    await screen.findByText(/交易所暂不可用/)

    await userEvent.click(await screen.findByRole('button', { name: '市价平仓' }))
    const again = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await userEvent.click(within(again).getByRole('button', { name: '市价平仓' }))

    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/close')).toBe(1)
    })
  })

  it('最差成交价按交易所的价格规则显示：最多 5 位有效数字', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({
            status: 'stopped',
            working_orders: 0,
            held: { position: '1.01', market_price: '104.73', read_at: '2026-10-15T00:05:00Z' },
          }),
          orders: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await userEvent.click(await screen.findByRole('button', { name: '市价平仓' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    // 104.73 × 0.95 = 99.4935，5 位有效数字。标记价恰好是整数时也不会少显示小数。
    expect(within(dialog).getByText('约 99.493')).toBeInTheDocument()
  })

  it('标记价恰好是整数时，最差成交价照样保留小数：105 × 0.95 = 99.75，不写成 100', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({
            status: 'stopped',
            working_orders: 0,
            held: { position: '0.68', market_price: '105', read_at: '2026-10-15T00:05:00Z' },
          }),
          orders: [],
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await userEvent.click(await screen.findByRole('button', { name: '市价平仓' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    expect(within(dialog).getByText('约 99.75')).toBeInTheDocument()
  })

  it('b50_第二次确认选保留仓位，就是普通停止，不调用平仓接口', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    await userEvent.click(within(dialog).getByRole('button', { name: '保留仓位' }))

    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument()
    expect(server.count('POST', '/api/strategies/s-1/close')).toBe(0)
    expect(screen.getByText(/仓位保留/)).toBeInTheDocument()
  })

  it('b58_停止时净仓位为 0 不问是否平仓', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { body: detail({ position: '0', expected_position: '0' }) },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    await userEvent.click(screen.getByRole('button', { name: '确认停止' }))

    expect(await screen.findByText(/仓位保留/)).toBeInTheDocument()
    expect(screen.queryByRole('alertdialog', { name: '市价平仓？' })).not.toBeInTheDocument()
  })

  it('b53_已归档、仍有仓位的策略可以市价平仓：空仓买回，最差成交价在标记价之上', async () => {
    const server = detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({
            status: 'stopped',
            working_orders: 0,
            held: { position: '-0.2', market_price: '95500', read_at: '2026-10-15T00:05:00Z' },
          }),
          orders: [],
          position: '0.015',
          expected_position: '0',
        }),
      },
      'POST /api/strategies/s-1/close': { body: { status: 'closing' } },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await userEvent.click(await screen.findByRole('button', { name: '市价平仓' }))
    const dialog = await screen.findByRole('alertdialog', { name: '市价平仓？' })
    // 空仓买回：95500 × 1.05。
    expect(within(dialog).getByText('约 100275')).toBeInTheDocument()
    await userEvent.click(within(dialog).getByRole('button', { name: '市价平仓' }))

    await waitFor(() => {
      expect(server.count('POST', '/api/strategies/s-1/close')).toBe(1)
    })
    expect(await screen.findByText(/已开始平仓/)).toBeInTheDocument()
  })

  it('b53_已归档、没有仍有仓位的读数就不给平仓按钮：归档时的旧快照不算数', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'stopped', working_orders: 0, held: null }),
          orders: [],
          position: '0.015',
          expected_position: '0',
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await expectTitle('做多', '已归档')
    expect(screen.queryByRole('button', { name: '市价平仓' })).not.toBeInTheDocument()
  })

  it('b56_平仓中：显示状态和清仓单，停止就是放弃剩下的平仓', async () => {
    const base = detail()
    const close = {
      ...base.orders[0],
      record: {
        ...base.orders[0].record,
        unit: { side: 'long' as const },
        purpose: 'close' as const,
        limit_price: '90725',
        quantity: '0.015',
        reduce_only: true,
      },
    }
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({
          summary: summary({ status: 'closing' }),
          orders: [close],
          note: '平仓中：还要平 0.015。',
        }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await expectTitle('做多', '平仓中')
    expect(screen.getByText('平仓中：还要平 0.015。')).toBeInTheDocument()
    expect(within(screen.getByRole('region', { name: '在场挂单' })).getByText('清仓')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '市价平仓' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: '停止' }))
    expect(screen.getByRole('alertdialog')).toHaveTextContent('放弃剩余平仓')
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

    expect(await screen.findByText(/撤单中/)).toBeInTheDocument()
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

    expect(await screen.findByText('已暂停，撤 2 张。')).toBeInTheDocument()
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

    expect(await screen.findByText(/撤单中/)).toBeInTheDocument()
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

    expect(await screen.findByText(/3 张遗留订单待收尾/)).toBeInTheDocument()
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

    expect(await screen.findByText('已归档，只读。')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '停止' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
    expect(screen.getByText('没有在场挂单。')).toBeInTheDocument()
    // 不在运行：没有在等的事，也没有「应有持仓」可比。
    expect(screen.queryByRole('region', { name: '等待' })).not.toBeInTheDocument()
    expect(screen.queryByText(/应有持仓/)).not.toBeInTheDocument()
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
    await expectTitle('做多', '失效中')
    expect(screen.getByRole('button', { name: '停止' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
  })

  it('b37_失效中的减仓单在挂单表里显示为整侧的失效减仓', async () => {
    const base = detail()
    const reduction = {
      ...base.orders[0],
      record: {
        ...base.orders[0].record,
        unit: { side: 'long' as const },
        purpose: 'invalidation' as const,
        limit_price: '69000',
        quantity: '0.4',
        reduce_only: true,
        status: 'submitting' as const,
      },
    }
    detailRoutes({
      'GET /api/strategies/s-1': {
        body: detail({ summary: summary({ status: 'invalidating' }), orders: [reduction] }),
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    const orders = await screen.findByRole('region', { name: '在场挂单' })
    expect(within(orders).getByText('多侧')).toBeInTheDocument()
    expect(within(orders).getByText('失效减仓')).toBeInTheDocument()
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
    expect(screen.getByText(/新建策略可接管剩余仓位/)).toBeInTheDocument()
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

    await expectTitle('做多', '已暂停')
    expect(screen.queryByRole('region', { name: '等待' })).not.toBeInTheDocument()
    expect(screen.queryByText(/应有持仓/)).not.toBeInTheDocument()
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

    await expectTitle('做多', '暂停中')
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

    await expectTitle('做多', '停止中')
    expect(
      screen.getByText('还有 1 张订单没撤掉、1 张订单的成交没对完账。'),
    ).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '暂停' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '恢复' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '停止' })).not.toBeInTheDocument()
    // 还没归档：仍然看得到在等什么。
    expect(screen.getByRole('region', { name: '等待' })).toBeInTheDocument()
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

    await expectTitle('做多', '恢复中')
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

    expect(await screen.findByText(/已暂停，恢复后可改限速和失效设置/)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '保存运行参数' })).not.toBeInTheDocument()
  })

  it('暂停中的策略可以恢复，恢复后说明挂了几张单', async () => {
    const server = detailRoutes({
      'GET /api/strategies/s-1': { body: detail({ summary: summary({ status: 'paused' }) }) },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.click(screen.getByRole('button', { name: '恢复' }))

    expect(await screen.findByText(/挂 1 张/)).toBeInTheDocument()
    expect(server.count('POST', '/api/strategies/s-1/resume')).toBe(1)
  })

  it('b44_运行中改限速间隔会提交整份参数，并显示新的版本号', async () => {
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/v3/)).toBeInTheDocument()
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

    expect(await screen.findByText(/v3/)).toBeInTheDocument()
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

    expect(await screen.findByText(/百分比数字/)).toBeInTheDocument()
    expect(server.count('PUT', '/api/strategies/s-1/config')).toBe(0)
  })

  it('b44_限速填的不是正数时不提交，也不把它当成不限速', async () => {
    // 把「写错了」当成「不限速」提交上去，用户以为限了速，实际提交的是另一份参数。
    const server = detailRoutes()
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '十分钟')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/填正数/)).toBeInTheDocument()
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

    expect(await screen.findByText(/v3/)).toBeInTheDocument()
    expect(server.lastBody('PUT', '/api/strategies/s-1/config')).toMatchObject({
      long: { throttle_interval: [90, 0] },
    })
  })

  it('b44_保存限速撞上服务端正在走一步时说清楚等一下再点', async () => {
    detailRoutes({
      'PUT /api/strategies/s-1/config': {
        status: 409,
        body: { kind: 'busy', error: '正在执行上一步，稍后再试' },
      },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)
    await screen.findByText('主仓批次表')

    await userEvent.type(screen.getByLabelText('做多侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/正在执行上一步，稍后再试/)).toBeInTheDocument()
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

    expect(await screen.findByText(/仓位核对失败，即将暂停/)).toBeInTheDocument()
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

    expect(await screen.findByText('仓位变动中')).toBeInTheDocument()
    expect(screen.queryByText(/即将暂停/)).not.toBeInTheDocument()
  })

  it('做空网格：类型、格子表和限速入口都按空头侧显示', async () => {
    detailRoutes({
      'GET /api/strategies/s-1': { body: shortDetail() },
    })
    renderWithQuery(<StrategyDetailView strategyId="s-1" onBack={() => {}} />)

    await expectTitle('做空', '运行中')
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(2)
    expect(rows[1]).toHaveTextContent('空0')
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

    await expectTitle('中性', '运行中')
    const table = screen.getByText('网格格子表').closest('table') as HTMLTableElement
    const rows = within(table).getAllByRole('row')
    expect(rows).toHaveLength(3)
    // 阶梯按建仓价从高到低：空头侧 91 在上，多头侧 89 在下。
    expect(rows[1]).toHaveTextContent('空0')
    expect(rows[2]).toHaveTextContent('多0')

    await userEvent.type(screen.getByLabelText('做空侧限速间隔 τ（分钟）'), '30')
    await userEvent.click(screen.getByRole('button', { name: '保存运行参数' }))

    expect(await screen.findByText(/v3/)).toBeInTheDocument()
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
    await userEvent.click(screen.getByRole('button', { name: /返回列表/ }))
    expect(onBack).toHaveBeenCalled()
  })
})
