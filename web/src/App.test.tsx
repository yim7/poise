import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { App } from './App'

const track = {
  identity: { id: 'btc-core', instrument: { venue: 'hyperliquid', symbol: 'BTC' } },
  status: { lifecycle: { status: 'active', updated_at: '2026-09-08T00:00:00Z' }, strategy_price: 80000, strategy_price_status: 'live' },
  strategy: { lower_price: 70_000, upper_price: 90_000, long_exposure_units: 2, short_exposure_units: 3, notional_per_unit: 125, out_of_band_policy: 'freeze' }, max_notional: 1000, loss_limits: {}, market: {},
  position: { current_exposure: 0, quantity_unit: 'base_asset', quantity: 0, notional: null, notional_asset: 'USD' }, position_to_execution_gap: 0,
  decision: { revision: 42, policy_target: -1.6612, risk_approved_target: -1.6612, acquisition_frontier: 0, execution_target: 0, reason: 'waiting_risk_release' },
  pnl: { pnl_asset: 'USD', gross_realized_pnl: 2, net_realized_pnl: 1.5, unrealized_pnl: -0.5, total_pnl: 1, trading_fee_cumulative: 0.2, funding_fee_cumulative: 0.1, completeness: 'complete', unvalued_cashflows: [] },
  execution: { state: 'open', execution_status: 'normal', active_binding_count: 2, attention_reasons: [], bindings: [{ id: 'a', label: 'maker', policy: 'curve_maker', status: 'working', intent: 'increase_inventory', order: { side: 'sell', price: 80100, quantity: 0.1 } }] }, activity: [], available_commands: []
}

const configuration = {
  version: 'config-7a0f',
  identity: { id: 'btc-core', instrument: { venue: 'hyperliquid', symbol: 'BTC' } },
  active: { lower_price: 70000, upper_price: 90000, long_exposure_units: 2, short_exposure_units: 3, notional_per_unit: 125, min_rebalance_units: 0.25, shape_family: 'linear', out_of_band_policy: 'freeze', max_notional: 1000, daily_loss_limit: 50, total_loss_limit: 150, risk_acquisition: { initial_ratio: 0.2, advantage_steps: 2, min_release_steps: 1, max_release_steps: 3, catchup_ratio: 0.5, stale_release_minutes: 60 } },
  restart_required: { leverage: 5, tick_timeout_secs: 15 }, application_status: 'idle',
}

class BrowserStream {
  static instances: BrowserStream[] = []
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null
  onmessage: ((event: MessageEvent<string>) => void) | null = null

  constructor() { BrowserStream.instances.push(this) }
  close() {}
  open() { this.onopen?.() }
  serverClose() { this.onclose?.() }
  emit(payload: object) { this.onmessage?.({ data: JSON.stringify(payload) } as MessageEvent<string>) }
}

function api(path: string) {
  if (path === '/readiness') return { ready_to_trade: true, blockers: [], warnings: [] }
  if (path === '/account') return { valuation_asset: 'USD', equity: 1200, available: 900, unrealized_pnl: 8, risk_signal: 'normal', updated_at: '2026-09-08T00:00:00Z' }
  if (path === '/tracks') return { items: [{ id: 'btc-core', instrument: track.identity.instrument, lifecycle: track.status.lifecycle, exposure: { current: 0 }, decision: track.decision, execution: track.execution, pnl: track.pnl }] }
  return track
}

function response(value: object, status = 200) {
  return new Response(JSON.stringify(value), { status })
}

describe('Poise read-only console', () => {
  afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals() })
  beforeEach(() => {
    BrowserStream.instances = []
    vi.stubGlobal('fetch', vi.fn((input: string) => Promise.resolve(new Response(JSON.stringify(api(input)), { status: 200 }))))
    vi.stubGlobal('WebSocket', BrowserStream)
  })

  it('shows a ready overview from the committed read endpoints', async () => {
    render(<App />)
    expect(screen.getByText('浏览器实时流正在连接')).toBeVisible()
    act(() => BrowserStream.instances[0].open())
    expect(screen.getByText('浏览器实时流已连接')).toBeVisible()
    expect(await screen.findByRole('heading', { name: '可以交易' })).toBeVisible()
    expect(screen.getByText('账户权益')).toBeVisible()
    expect(screen.getByText('btc-core')).toBeVisible()
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('/tracks'))
  })

  it('updates the selected run page from committed live events', async () => {
    render(<App />)
    await screen.findByText('btc-core')
    screen.getAllByRole('button', { name: /btc-core/ })[0].click()
    await screen.findByRole('heading', { name: '等待风险释放' })

    const live = {
        strategy_price: 79_800, strategy_price_status: 'live', mark_price: 79_810, best_bid: 79_805, best_ask: 79_815,
        position_to_execution_gap: -1.25,
        decision: { revision: 43, policy_target: -3, risk_approved_target: -3, acquisition_frontier: -1.25, execution_target: -1.25, reason: 'price_gate_blocked' },
        acquisition_session: { acquisition_reset_reason: 'fresh_session_start', acquisition_session_started_at: '2026-09-08T00:00:00Z', initial_frontier: 0, initial_release_units: 0 },
        risk_acquisition: { direction: 'short', backlog_units: 1.75, release_anchor_price: 80_000, release_anchor_target: -3, stale_release_elapsed_minutes: 17, stale_release_minutes: 60, next_advantage_target: -3.5, next_advantage_price: 79_500, next_release_units: 0.25, next_release_target: -1.5 },
        price_execution_block_reason: 'mark_book_divergence'
    }
    act(() => BrowserStream.instances.at(-1)!.emit({ type: 'track_live_view_changed', track_id: 'eth-core', live }))
    expect(screen.getByRole('heading', { name: '等待风险释放' })).toBeVisible()
    act(() => BrowserStream.instances.at(-1)!.emit({ type: 'track_live_view_changed', track_id: 'btc-core', live }))

    expect(screen.getByRole('heading', { name: '价格保护阻止执行' })).toBeVisible()
    expect(screen.getByText('策略目标').nextElementSibling).toHaveTextContent('-3.0000')
    expect(screen.getByText('实际到执行差额').nextElementSibling).toHaveTextContent('-1.2500')
    expect(screen.getByText('待释放').nextElementSibling).toHaveTextContent('空头 1.7500')
    expect(screen.getByText('下一有利价格').nextElementSibling).toHaveTextContent('79,500.00')
    expect(screen.getByText('风险释放等待').nextElementSibling).toHaveTextContent('已等待 17.0 分钟 / 上限 60.0 分钟')
    expect(screen.getByText('下一次释放').nextElementSibling).toHaveTextContent('0.2500')
    expect(screen.getByText('释放后的执行目标').nextElementSibling).toHaveTextContent('-1.5000')
    expect(screen.getByText('价格区间').nextElementSibling).toHaveTextContent('70,000.00 – 90,000.00')
    expect(screen.getByText('最大多空 exposure').nextElementSibling).toHaveTextContent('多 2.0000 / 空 3.0000')
    expect(screen.getByText('每单位名义金额').nextElementSibling).toHaveTextContent('125.0000')
    expect(screen.getByText('区间外策略').nextElementSibling).toHaveTextContent('freeze')
  })

  it('updates an unselected Track summary from its committed live event', async () => {
    render(<App />)
    await screen.findByText('btc-core')
    act(() => BrowserStream.instances[0].emit({ type: 'track_live_view_changed', track_id: 'btc-core', live: {
      strategy_price: 81_000, strategy_price_status: 'live', mark_price: 81_000, best_bid: 80_999, best_ask: 81_001, position_to_execution_gap: 2.2222,
      decision: { revision: 90, policy_target: 2.2222, risk_approved_target: 2.2222, acquisition_frontier: 2.2222, execution_target: 2.2222, reason: 'at_execution_target' },
      acquisition_session: null, risk_acquisition: null, price_execution_block_reason: null,
    }}))
    expect(screen.getByText('可执行目标 2.2222')).toBeVisible()
    expect(screen.getByText('已达到执行目标')).toBeVisible()
  })

  it('reconnects with backoff and reloads visible committed state after open', async () => {
    let reconnected = false
    vi.stubGlobal('fetch', vi.fn((input: string) => Promise.resolve(response(input === '/readiness' && reconnected
      ? { ready_to_trade: false, blockers: [{ component: 'market_data', reason: '行情恢复中' }], warnings: [] }
      : api(input)))))
    render(<App />)
    await screen.findByRole('heading', { name: '可以交易' })
    act(() => BrowserStream.instances[0].open())
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(6))
    vi.useFakeTimers()
    reconnected = true
    act(() => BrowserStream.instances[0].serverClose())
    expect(screen.getByText('浏览器实时流已断开')).toBeVisible()
    await act(async () => { await vi.advanceTimersByTimeAsync(1000) })
    expect(BrowserStream.instances).toHaveLength(2)
    act(() => BrowserStream.instances[1].open())
    await act(async () => { await Promise.resolve() })
    expect(screen.getByRole('heading', { name: '暂不可交易' })).toBeVisible()
    expect(screen.getByText('行情恢复中')).toBeVisible()
  })

  it('polls readiness without polling Track detail', async () => {
    let poll!: () => void
    let readinessCalls = 0
    vi.stubGlobal('setInterval', vi.fn((callback: TimerHandler, delay?: number) => { if (delay === 10_000) poll = callback as () => void; return 7 }))
    vi.stubGlobal('clearInterval', vi.fn())
    vi.stubGlobal('fetch', vi.fn((input: string) => {
      if (input === '/readiness') {
        readinessCalls += 1
        if (readinessCalls === 2) return Promise.reject(new Error('temporary poll failure'))
        return Promise.resolve(response(readinessCalls === 1 ? api(input) : { ready_to_trade: false, blockers: [{ component: 'account', reason: '账户状态更新中' }], warnings: [] }))
      }
      return Promise.resolve(response(api(input)))
    }))
    render(<App />)
    await screen.findByRole('heading', { name: '可以交易' })
    await act(async () => { poll(); await new Promise(resolve => setTimeout(resolve, 0)) })
    expect(screen.getByText('无法读取运行状态')).toBeVisible()
    await act(async () => { poll(); await new Promise(resolve => setTimeout(resolve, 0)) })
    expect(screen.getByRole('heading', { name: '暂不可交易' })).toBeVisible()
    expect(screen.getByText('账户状态更新中')).toBeVisible()
    expect(screen.queryByText('无法读取运行状态')).not.toBeInTheDocument()
    expect(fetch).not.toHaveBeenCalledWith('/tracks/btc-core', expect.anything())
  })

  it('shows loading immediately and ignores a late detail response after switching tracks', async () => {
    let resolveEth!: (response: Response) => void
    const ethRequest = new Promise<Response>(resolve => { resolveEth = resolve })
    const eth = { ...track, identity: { id: 'eth-core', instrument: { venue: 'hyperliquid', symbol: 'ETH' } } }
    vi.stubGlobal('fetch', vi.fn((input: string) => {
      if (input === '/tracks') return Promise.resolve(response({ items: [
        { id: 'btc-core', instrument: track.identity.instrument, lifecycle: track.status.lifecycle, exposure: { current: 0 }, decision: track.decision, execution: track.execution, pnl: track.pnl },
        { id: 'eth-core', instrument: eth.identity.instrument, lifecycle: eth.status.lifecycle, exposure: { current: 0 }, decision: eth.decision, execution: eth.execution, pnl: eth.pnl },
      ] }))
      if (input === '/tracks/eth-core') return ethRequest
      return Promise.resolve(response(api(input)))
    }))
    render(<App />)
    await screen.findByRole('button', { name: /btc-core BTC/ })
    fireEvent.click(screen.getByRole('button', { name: /btc-core BTC/ }))
    await screen.findByText(/hyperliquid · BTC/)

    fireEvent.click(screen.getByRole('button', { name: /eth-core ETH/ }))
    expect(screen.getByRole('heading', { name: 'Track 载入中' })).toBeVisible()
    fireEvent.click(screen.getByRole('button', { name: /btc-core BTC/ }))
    await screen.findByText(/hyperliquid · BTC/)
    await act(async () => resolveEth(response(eth)))
    expect(screen.getByText(/hyperliquid · BTC/)).toBeVisible()
    expect(screen.queryByText(/hyperliquid · ETH/)).not.toBeInTheDocument()
  })

  it('retries the selected Track detail after its GET fails', async () => {
    let attempts = 0
    vi.stubGlobal('fetch', vi.fn((input: string) => {
      if (input === '/tracks/btc-core') {
        attempts += 1
        return Promise.resolve(attempts === 1 ? response({ error: 'detail unavailable' }, 503) : response(track))
      }
      return Promise.resolve(response(api(input)))
    }))
    render(<App />)
    await screen.findByText('btc-core')
    screen.getAllByRole('button', { name: /btc-core/ })[0].click()
    expect(await screen.findByText('无法读取运行状态')).toBeVisible()
    screen.getByRole('button', { name: '重试' }).click()
    expect(await screen.findByText(/hyperliquid · BTC/)).toBeVisible()
    expect(attempts).toBe(2)
  })

  it('explains waiting risk release and presents the target chain on the selected track', async () => {
    render(<App />)
    await screen.findByText('btc-core')
    screen.getAllByRole('button', { name: /btc-core/ })[0].click()
    expect(await screen.findByRole('heading', { name: '等待风险释放' })).toBeVisible()
    expect(screen.getByText('策略目标')).toBeVisible()
    expect(screen.getByText('风险批准目标')).toBeVisible()
    expect(screen.getByText('风险释放边界')).toBeVisible()
    expect(screen.getByText('当前可执行目标')).toBeVisible()
    expect(screen.getByText(/风险目标已经批准/)).toBeVisible()
    expect(screen.getByText('诊断详情').parentElement).not.toHaveAttribute('open')
  })

  it('reports unavailable reads and a disconnected stream', async () => {
    vi.stubGlobal('fetch', vi.fn(() => Promise.reject(new Error('offline'))))
    vi.stubGlobal('WebSocket', class { onopen: (() => void) | null = null; onmessage: (() => void) | null = null; onerror: (() => void) | null = null; onclose: (() => void) | null = null; constructor() { queueMicrotask(() => this.onerror?.()) }; close() {} })
    render(<App />)
    expect(await screen.findByText('无法读取运行状态')).toBeVisible()
    expect(screen.getByText('浏览器实时流已断开')).toBeVisible()
  })

  it('loads configuration only after entering the tab, keeps a complete editable draft, and previews it', async () => {
    const preview = { previewed_at: '2026-09-08T12:00:00Z', valid: true, field_errors: [], blockers: [], impact: { strategy_price: 80000, position_quantity: 0.1, position_quantity_unit: 'base_asset', current_exposure: 0, candidate_current_exposure: 0, active_policy_target: 1, candidate_curve_target: 2, candidate_policy_target: 2, candidate_policy_state: 'in_band', candidate_risk_approved_target: 2, candidate_risk_outcome: 'allowed', active_policy_target_position_qty: 0.1, candidate_policy_target_position_qty: 0.2, target_position_qty_delta: 0.1, active_policy_target_signed_notional: 125, candidate_policy_target_signed_notional: 250, target_signed_notional_delta: 125, candidate_target_delta_from_current_exposure: 2, candidate_position_qty_delta_from_current: 0.2, initial_frontier: 0.2, initial_execution_target: 0.2, position_qty_per_unit: 0.1, minimum_rebalance_position_qty: 0.01, normalized_minimum_rebalance_position_qty: 0.01, minimum_rebalance_notional: 10, maximum_target_position_qty: 1, notional_asset: 'USD' }, capacity_proof: { available_increase_notional: 800, required_increase_notional: 125, asset: 'USD', observed_at: '2026-09-08T12:00:00Z' } }
    let previewBody = ''
    vi.stubGlobal('fetch', vi.fn((input: string, init?: RequestInit) => {
      if (input === '/tracks/btc-core/configuration/preview') {
        previewBody = String(init?.body); return Promise.resolve(response(preview))
      }
      if (input === '/tracks/btc-core/configuration') {
        return Promise.resolve(response(configuration))
      }
      return Promise.resolve(response(api(input)))
    }))
    render(<App />)
    await screen.findByText('btc-core')
    fireEvent.click(screen.getAllByRole('button', { name: /btc-core/ })[0])
    await screen.findByText(/hyperliquid · BTC/)
    expect(fetch).not.toHaveBeenCalledWith('/tracks/btc-core/configuration', expect.anything())
    fireEvent.click(screen.getByRole('tab', { name: '配置' }))
    expect(await screen.findByText('当前生效值')).toBeVisible()
    expect(screen.getByText('配置应用状态 · 空闲')).toBeVisible()
    const lower = screen.getByLabelText('下限价格') as HTMLInputElement
    fireEvent.change(lower, { target: { value: '71000' } })
    expect(screen.getByText('有未保存修改')).toBeVisible()
    fireEvent.click(screen.getByRole('button', { name: '重新预览' }))
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('/tracks/btc-core/configuration/preview', expect.objectContaining({ method: 'POST', body: expect.stringContaining('71000') })))
    expect(JSON.parse(previewBody)).toEqual({ candidate: { ...configuration.active, lower_price: 71000 } })
    expect(await screen.findByText('预览结果')).toBeVisible()
    expect(screen.getByText('实际数量不变')).toBeVisible()
    expect(screen.getByText(/候选 exposure/)).toBeVisible()
    expect(screen.getByText('容量证明')).toBeVisible()
    expect(screen.getByText('每 exposure 单位对应数量')).toBeVisible()
    expect(screen.getByText('场所最小调整数量')).toBeVisible()
    expect(screen.getByText('归一化最小调整数量')).toBeVisible()
    expect(screen.getByText('最小调整名义金额')).toBeVisible()
    expect(screen.getByText('最大目标数量')).toBeVisible()
    expect(screen.getByText('观测时间').nextElementSibling).toHaveTextContent('2026-09-08T12:00:00Z')
    expect(screen.getByText('候选策略目标 / 状态').nextElementSibling).toHaveTextContent('区间内')
    expect(screen.getByText('候选风险批准目标 / 结果').nextElementSibling).toHaveTextContent('允许')
    expect(screen.getByRole('button', { name: '保存并应用' })).toBeDisabled()
    expect(screen.getByText(/将在下一实施阶段开放/)).toBeVisible()
  })

  it('shows preview errors, invalidates the old preview on edits, and discards back to active values', async () => {
    const failedPreview = { previewed_at: '2026-09-08T12:00:00Z', valid: false, field_errors: [{ field: 'upper_price', message: '必须大于下限价格' }], blockers: [{ code: 'risk_limit', message: '超过风险限制' }] }
    vi.stubGlobal('fetch', vi.fn((input: string) => input === '/tracks/btc-core/configuration/preview'
      ? Promise.resolve(response(failedPreview)) : input === '/tracks/btc-core/configuration' ? Promise.resolve(response(configuration)) : Promise.resolve(response(api(input)))))
    render(<App />)
    await screen.findByText('btc-core'); fireEvent.click(screen.getAllByRole('button', { name: /btc-core/ })[0]); await screen.findByText(/hyperliquid · BTC/)
    fireEvent.click(screen.getByRole('tab', { name: '配置' })); await screen.findByText('当前生效值')
    fireEvent.click(screen.getByRole('button', { name: '重新预览' }))
    expect(await screen.findByText('必须大于下限价格')).toBeVisible(); expect(screen.getByText('超过风险限制')).toBeVisible()
    fireEvent.change(screen.getByLabelText('上限价格'), { target: { value: '91000' } })
    expect(screen.queryByText('必须大于下限价格')).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '放弃修改' }))
    expect((screen.getByLabelText('上限价格') as HTMLInputElement).value).toBe('90000')
    expect(screen.queryByText('有未保存修改')).not.toBeInTheDocument()
  })

  it('ignores a stale configuration response after switching Tracks', async () => {
    let resolveBtc!: (value: Response) => void
    const delayed = new Promise<Response>(resolve => { resolveBtc = resolve })
    const eth = { ...track, identity: { id: 'eth-core', instrument: { venue: 'hyperliquid', symbol: 'ETH' } } }
    vi.stubGlobal('fetch', vi.fn((input: string) => {
      if (input === '/tracks') return Promise.resolve(response({ items: [
        { id: 'btc-core', instrument: track.identity.instrument, lifecycle: track.status.lifecycle, exposure: { current: 0 }, decision: track.decision, execution: track.execution, pnl: track.pnl },
        { id: 'eth-core', instrument: eth.identity.instrument, lifecycle: eth.status.lifecycle, exposure: { current: 0 }, decision: eth.decision, execution: eth.execution, pnl: eth.pnl },
      ] }))
      if (input === '/tracks/btc-core/configuration') return delayed
      return Promise.resolve(response(input === '/tracks/eth-core' ? eth : api(input)))
    }))
    render(<App />); await screen.findByRole('button', { name: /btc-core BTC/ }); fireEvent.click(screen.getByRole('button', { name: /btc-core BTC/ })); await screen.findByText(/hyperliquid · BTC/)
    fireEvent.click(screen.getByRole('tab', { name: '配置' })); expect(screen.getByText('配置载入中')).toBeVisible()
    fireEvent.click(screen.getByRole('button', { name: /eth-core ETH/ })); await screen.findByText(/hyperliquid · ETH/)
    await act(async () => resolveBtc(response(configuration)))
    expect(screen.queryByText('当前生效值')).not.toBeInTheDocument()
  })

  it('serializes the protocol flatten object from structured controls', async () => {
    let previewBody = ''
    vi.stubGlobal('fetch', vi.fn((input: string, init?: RequestInit) => {
      if (input === '/tracks/btc-core/configuration/preview') { previewBody = String(init?.body); return Promise.resolve(response({ previewed_at: '2026-09-09T00:00:00Z', valid: false, field_errors: [], blockers: [], impact: null, capacity_proof: null })) }
      return Promise.resolve(response(input === '/tracks/btc-core/configuration' ? configuration : api(input)))
    }))
    render(<App />); await screen.findByText('btc-core'); fireEvent.click(screen.getAllByRole('button', { name: /btc-core/ })[0]); await screen.findByText(/hyperliquid · BTC/)
    fireEvent.click(screen.getByRole('tab', { name: '配置' })); await screen.findByText('当前生效值')
    fireEvent.change(screen.getByLabelText('区间外策略'), { target: { value: 'flatten' } })
    expect((screen.getByLabelText('Flatten 确认 bps') as HTMLInputElement).value).toBe('500')
    expect((screen.getByLabelText('重新进入确认 bps') as HTMLInputElement).value).toBe('500')
    fireEvent.click(screen.getByRole('button', { name: '重新预览' }))
    await waitFor(() => expect(previewBody).not.toBe(''))
    expect(JSON.parse(previewBody).candidate.out_of_band_policy).toEqual({ flatten: { trigger: { flatten_confirm: { bps: 500 } }, recover: { reentry_confirm: { bps: 500 } } } })
  })

  it('ignores a delayed preview after an edit or Track switch', async () => {
    let resolveFirst!: (value: Response) => void
    let resolveSecond!: (value: Response) => void
    const first = new Promise<Response>(resolve => { resolveFirst = resolve })
    const second = new Promise<Response>(resolve => { resolveSecond = resolve })
    let previewCalls = 0
    const eth = { ...track, identity: { id: 'eth-core', instrument: { venue: 'hyperliquid', symbol: 'ETH' } } }
    const delayedPreview = { previewed_at: '2026-09-09T00:00:00Z', valid: true, field_errors: [], blockers: [], impact: null, capacity_proof: null }
    vi.stubGlobal('fetch', vi.fn((input: string) => {
      if (input === '/tracks') return Promise.resolve(response({ items: [
        { id: 'btc-core', instrument: track.identity.instrument, lifecycle: track.status.lifecycle, exposure: { current: 0 }, decision: track.decision, execution: track.execution, pnl: track.pnl },
        { id: 'eth-core', instrument: eth.identity.instrument, lifecycle: eth.status.lifecycle, exposure: { current: 0 }, decision: eth.decision, execution: eth.execution, pnl: eth.pnl },
      ] }))
      if (input === '/tracks/btc-core/configuration') return Promise.resolve(response(configuration))
      if (input === '/tracks/btc-core/configuration/preview') return ++previewCalls === 1 ? first : second
      if (input === '/tracks/eth-core') return Promise.resolve(response(eth))
      return Promise.resolve(response(api(input)))
    }))
    render(<App />); await screen.findByRole('button', { name: /btc-core BTC/ }); fireEvent.click(screen.getByRole('button', { name: /btc-core BTC/ })); await screen.findByText(/hyperliquid · BTC/)
    fireEvent.click(screen.getByRole('tab', { name: '配置' })); await screen.findByText('当前生效值')
    fireEvent.click(screen.getByRole('button', { name: '重新预览' })); fireEvent.change(screen.getByLabelText('下限价格'), { target: { value: '71000' } })
    await act(async () => resolveFirst(response(delayedPreview)))
    expect(screen.queryByText('预览结果')).not.toBeInTheDocument()
    fireEvent.click(screen.getByRole('button', { name: '重新预览' })); fireEvent.click(screen.getByRole('button', { name: /eth-core ETH/ })); await screen.findByText(/hyperliquid · ETH/)
    await act(async () => resolveSecond(response(delayedPreview)))
    expect(screen.queryByText('预览结果')).not.toBeInTheDocument()
  })
})
