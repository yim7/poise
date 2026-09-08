import { useEffect, useMemo, useRef, useState } from 'react'
import type { AccountSummaryView, ExecutionBindingView, StreamEvent, TrackDetailView, TrackListItemView, TrackListResponse, TradingReadinessResponse } from './types'

const chineseReason: Record<string, { title: string; detail: string }> = {
  waiting_risk_release: { title: '等待风险释放', detail: '策略希望建立目标仓位。风险目标已经批准，但当前 session 尚未释放新增风险，因此暂不下单。' },
  waiting_market_data: { title: '等待行情数据', detail: '尚未取得可用于决策的行情。' },
  paused: { title: '已暂停', detail: 'Track 已暂停，不会发起新的执行。' },
  risk_input_invalid: { title: '风险输入无效', detail: '风险输入尚未满足执行条件。' },
  band_policy_blocked: { title: '价格区间策略阻止执行', detail: '当前价格区间策略不允许继续执行。' },
  at_execution_target: { title: '已达到执行目标', detail: '实际仓位已经到达当前可执行目标。' },
  account_capacity_blocked: { title: '账户容量不足', detail: '账户容量限制阻止新增仓位。' },
  price_gate_blocked: { title: '价格保护阻止执行', detail: '当前报价不满足执行价格保护。' },
  below_rebalance: { title: '低于最小调整量', detail: '当前差额不足以触发再平衡。' },
  below_venue_minimum: { title: '低于交易所最小值', detail: '当前调整不满足交易所最小下单要求。' },
  covered_by_working_order: { title: '已有工作订单覆盖', detail: '现有工作订单正在覆盖当前执行意图。' },
  recovery_required: { title: '需要恢复确认', detail: '执行状态需要先完成恢复与对账。' },
  waiting_cancel: { title: '等待撤单', detail: '系统正在等待现有订单撤销完成。' },
  submit_planned: { title: '准备提交订单', detail: '系统已规划下一步执行，正在等待 executor 处理。' },
}

function format(value: unknown, digits = 4) {
  return typeof value === 'number' ? value.toLocaleString('zh-CN', { maximumFractionDigits: digits, minimumFractionDigits: digits }) : '—'
}

function title(value: unknown) {
  return typeof value === 'string' ? value.replaceAll('_', ' ') : '—'
}

function policy(value: string | Record<string, unknown>) {
  return typeof value === 'string' ? title(value) : JSON.stringify(value)
}

async function get<T>(path: string, init?: RequestInit) {
  const response = init ? await fetch(path, init) : await fetch(path)
  if (!response.ok) throw new Error(`${path} ${response.status}`)
  return response.json() as Promise<T>
}

function websocketUrl() {
  return `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
}

export function App() {
  const [readiness, setReadiness] = useState<TradingReadinessResponse | null>(null)
  const [account, setAccount] = useState<AccountSummaryView | null>(null)
  const [tracks, setTracks] = useState<TrackListItemView[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [detail, setDetail] = useState<TrackDetailView | null>(null)
  const [detailRequestVersion, setDetailRequestVersion] = useState(0)
  const selectedIdRef = useRef<string | null>(null)
  const [error, setError] = useState(false)
  const [readinessPollError, setReadinessPollError] = useState(false)
  const [connection, setConnection] = useState<'connecting' | 'connected' | 'disconnected'>('connecting')
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null)

  const refresh = async () => {
    try {
      const [nextReadiness, nextAccount, trackList] = await Promise.all([
        get<TradingReadinessResponse>('/readiness'),
        get<AccountSummaryView>('/account'),
        get<TrackListResponse>('/tracks'),
      ])
      setReadiness(nextReadiness); setAccount(nextAccount); setTracks(trackList.items ?? []); setError(false); setUpdatedAt(new Date())
    } catch { setError(true) }
  }

  useEffect(() => { void refresh() }, [])
  useEffect(() => {
    let stopped = false
    let attempts = 0
    let reconnectTimer: number | null = null
    let socket: WebSocket | null = null

    const disconnect = (current: WebSocket) => {
      if (stopped || current !== socket) return
      current.onerror = null
      current.onclose = null
      current.onmessage = null
      current.close()
      if (reconnectTimer !== null) return
      setConnection('disconnected')
      const delay = Math.min(1000 * 2 ** attempts, 10_000)
      attempts += 1
      reconnectTimer = window.setTimeout(() => {
        reconnectTimer = null
        connect()
      }, delay)
    }

    const connect = () => {
      if (stopped) return
      setConnection('connecting')
      const current = new WebSocket(websocketUrl())
      socket = current
      current.onopen = () => {
        attempts = 0
        if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
        reconnectTimer = null
        setConnection('connected')
        void refresh()
        if (selectedIdRef.current) {
          setDetail(null)
          setDetailRequestVersion(version => version + 1)
        }
      }
      current.onerror = () => disconnect(current)
      current.onclose = () => disconnect(current)
      current.onmessage = (event) => {
        try {
          const update = JSON.parse(event.data) as StreamEvent
          if (update.type === 'track_list_item_changed') setTracks(previous => previous.map(item => item.id === update.track_id ? update.item : item))
          if (update.type === 'track_detail_changed') setDetail(current => current?.identity.id === update.track_id ? update.detail : current)
          if (update.type === 'track_live_view_changed') {
            setTracks(previous => previous.map(item => item.id === update.track_id ? {
              ...item,
              strategy_price: update.live.strategy_price,
              strategy_price_status: update.live.strategy_price_status,
              decision: update.live.decision,
              execution: { ...item.execution, price_execution_block_reason: update.live.price_execution_block_reason },
            } : item))
            setDetail(current => current?.identity.id === update.track_id ? {
              ...current,
              status: { ...current.status, strategy_price: update.live.strategy_price, strategy_price_status: update.live.strategy_price_status },
              market: { mark_price: update.live.mark_price, best_bid: update.live.best_bid, best_ask: update.live.best_ask },
              position_to_execution_gap: update.live.position_to_execution_gap,
              decision: update.live.decision,
              execution: { ...current.execution, acquisition_session: update.live.acquisition_session, risk_acquisition: update.live.risk_acquisition, price_execution_block_reason: update.live.price_execution_block_reason },
            } : current)
          }
          if (update.type === 'account_summary_changed') setAccount(update.summary)
          setUpdatedAt(new Date())
        } catch { disconnect(current) }
      }
    }
    connect()
    return () => {
      stopped = true
      if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
      if (socket) { socket.onclose = null; socket.onerror = null; socket.close() }
    }
  }, [])
  useEffect(() => {
    const timer = window.setInterval(() => {
      void get<TradingReadinessResponse>('/readiness').then(value => { setReadiness(value); setReadinessPollError(false); setUpdatedAt(new Date()) }).catch(() => setReadinessPollError(true))
    }, 10_000)
    return () => window.clearInterval(timer)
  }, [])
  useEffect(() => {
    if (!selectedId) { setDetail(null); return }
    const controller = new AbortController()
    get<TrackDetailView>(`/tracks/${encodeURIComponent(selectedId)}`, { signal: controller.signal })
      .then(value => {
        if (controller.signal.aborted) return
        setDetail(value); setError(false); setUpdatedAt(new Date())
      })
      .catch(() => { if (!controller.signal.aborted) setError(true) })
    return () => controller.abort()
  }, [selectedId, detailRequestVersion])

  const selectTrack = (id: string | null) => {
    selectedIdRef.current = id
    setDetail(null)
    setError(false)
    setSelectedId(id)
  }
  const retry = () => {
    setReadinessPollError(false)
    if (selectedId) {
      setError(false)
      setDetail(null)
      setDetailRequestVersion(version => version + 1)
    } else {
      void refresh()
    }
  }

  const status = readiness === null ? '交易条件载入中' : readiness.ready_to_trade ? '可以交易' : '暂不可交易'
  const connectionLabel = connection === 'connecting' ? '浏览器实时流正在连接' : connection === 'connected' ? '浏览器实时流已连接' : '浏览器实时流已断开'
  const lastUpdated = useMemo(() => updatedAt ? updatedAt.toLocaleTimeString('zh-CN') : '—', [updatedAt])

  return <main>
    <header className="topbar"><div><strong>Poise</strong><span>{status}</span></div><div><span className={connection === 'connected' ? 'connection online' : 'connection'}>{connectionLabel}</span><span>更新于 {lastUpdated}</span></div></header>
    {(error || readinessPollError) && <div role="alert" className="alert"><strong>无法读取运行状态</strong><span>请确认本地 server 正在运行后重试。</span><button onClick={retry}>重试</button></div>}
    <div className="layout"><aside><button className={!selectedId ? 'nav active' : 'nav'} onClick={() => selectTrack(null)}>总览</button><p>Tracks</p>{tracks.map(track => <button key={track.id} className={selectedId === track.id ? 'track active' : 'track'} onClick={() => selectTrack(track.id)}><strong>{track.id}</strong><small>{track.instrument?.symbol} · {title(track.lifecycle?.status)}</small></button>)}</aside>
    <section className="content">{selectedId ? detail ? <TrackPage detail={detail} /> : <h1>Track 载入中</h1> : <Overview readiness={readiness} account={account} tracks={tracks} onSelect={selectTrack} />}</section></div>
  </main>
}

function Overview({ readiness, account, tracks, onSelect }: { readiness: TradingReadinessResponse | null; account: AccountSummaryView | null; tracks: TrackListItemView[]; onSelect: (id: string) => void }) {
  const blockers = readiness?.blockers ?? []
  const status = readiness === null ? '交易条件载入中' : readiness.ready_to_trade ? '可以交易' : '暂不可交易'
  return <><h1>{status}</h1>{readiness !== null && !readiness.ready_to_trade && blockers[0] && <p className="explanation">{blockers[0].reason}</p>}
    <div className="cards"><article><h2>账户权益</h2><strong>{format(account?.equity)} {account?.valuation_asset ?? ''}</strong></article><article><h2>可用保证金</h2><strong>{format(account?.available)} {account?.valuation_asset ?? ''}</strong></article><article><h2>风险信号</h2><strong>{title(account?.risk_signal)}</strong></article></div>
    <h2>Track</h2><div className="track-grid">{tracks.map(track => <button className="summary" key={track.id} onClick={() => onSelect(track.id)}><strong>{track.id} · {title(track.lifecycle?.status)}</strong><span>当前仓位 {format(track.exposure?.current)}</span><span>可执行目标 {format(track.decision?.execution_target)}</span><small>{chineseReason[track.decision?.reason]?.title ?? title(track.decision?.reason)}</small></button>)}</div>
  </>
}

function TrackPage({ detail }: { detail: TrackDetailView }) {
  const decision = detail.decision ?? {}; const reason = chineseReason[decision.reason] ?? { title: title(decision.reason), detail: '请在诊断详情中查看原始状态。' }
  const targets = [['实际仓位', detail.position?.current_exposure], ['策略目标', decision.policy_target], ['风险批准目标', decision.risk_approved_target], ['风险释放边界', decision.acquisition_frontier], ['当前可执行目标', decision.execution_target]]
  return <><p className="eyebrow">{detail.identity?.instrument?.venue} · {detail.identity?.instrument?.symbol} · {title(detail.status?.lifecycle?.status)}</p><h1>{reason.title}</h1><p className="explanation">{reason.detail}</p>
    <article><h2>仓位与目标</h2><dl className="targets">{targets.map(([label, value]) => <div key={String(label)}><dt>{label}</dt><dd>{format(value)}</dd></div>)}<div><dt>实际到执行差额</dt><dd>{format(detail.position_to_execution_gap)}</dd></div></dl></article>
    {detail.execution.risk_acquisition && <article className="next-step"><h2>下一步条件</h2><dl><div><dt>待释放</dt><dd>{detail.execution.risk_acquisition.direction === 'long' ? '多头' : detail.execution.risk_acquisition.direction === 'short' ? '空头' : title(detail.execution.risk_acquisition.direction)} {format(detail.execution.risk_acquisition.backlog_units)}</dd></div><div><dt>下一有利价格</dt><dd>{format(detail.execution.risk_acquisition.next_advantage_price, 2)}</dd></div><div><dt>风险释放等待</dt><dd>已等待 {format(detail.execution.risk_acquisition.stale_release_elapsed_minutes, 1)} 分钟 / 上限 {format(detail.execution.risk_acquisition.stale_release_minutes, 1)} 分钟</dd></div><div><dt>下一次释放</dt><dd>{format(detail.execution.risk_acquisition.next_release_units)}</dd></div><div><dt>释放后的执行目标</dt><dd>{format(detail.execution.risk_acquisition.next_release_target)}</dd></div></dl></article>}
    <article className="config-summary"><h2>配置摘要</h2><dl><div><dt>价格区间</dt><dd>{format(detail.strategy.lower_price, 2)} – {format(detail.strategy.upper_price, 2)}</dd></div><div><dt>最大多空 exposure</dt><dd>多 {format(detail.strategy.long_exposure_units)} / 空 {format(detail.strategy.short_exposure_units)}</dd></div><div><dt>每单位名义金额</dt><dd>{format(detail.strategy.notional_per_unit)}</dd></div><div><dt>区间外策略</dt><dd>{policy(detail.strategy.out_of_band_policy)}</dd></div><div><dt>最大名义金额</dt><dd>{format(detail.max_notional)}</dd></div></dl></article>
    <div className="two-column"><article><h2>Active bindings</h2>{detail.execution.bindings.length ? <ul>{detail.execution.bindings.map((binding: ExecutionBindingView) => <li key={binding.id}><strong>{binding.label}</strong><span>{title(binding.status)} · {title(binding.intent)}</span>{binding.order && <span>{title(binding.order.side)} {format(binding.order.quantity)} @ {format(binding.order.price, 2)}</span>}</li>)}</ul> : <p>当前没有 active binding。</p>}</article>
    <article><h2>PNL 摘要</h2><dl><div><dt>总 PNL</dt><dd>{format(detail.pnl?.total_pnl)} {detail.pnl?.pnl_asset ?? ''}</dd></div><div><dt>完整性</dt><dd>{title(detail.pnl?.completeness)}</dd></div>{(detail.pnl?.unvalued_cashflows ?? []).length > 0 && <div><dt>未估值 cashflow</dt><dd>{detail.pnl.unvalued_cashflows.length}</dd></div>}</dl></article></div>
    <details><summary>诊断详情</summary><dl><div><dt>Decision reason</dt><dd>{title(decision.reason)}</dd></div><div><dt>Decision revision</dt><dd>{decision.revision ?? '—'}</dd></div><div><dt>生命周期更新时间</dt><dd>{detail.status?.lifecycle?.updated_at ?? '—'}</dd></div></dl></details>
  </>
}
