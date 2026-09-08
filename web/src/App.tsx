import { useEffect, useMemo, useRef, useState } from 'react'
import type { AccountSummaryView, BandFlattenTrigger, BandProtectionPolicy, BandRecoverPolicy, ExecutionBindingView, StreamEvent, TrackActivityItemView, TrackConfigurationActiveView, TrackConfigurationApplicationStatus, TrackConfigurationApplicationView, TrackConfigurationPreviewResponse, TrackConfigurationResponse, TrackDetailView, TrackListItemView, TrackListResponse, TradingReadinessResponse } from './types'

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

function applicationStatusLabel(value: TrackConfigurationResponse['application_status']) {
  return ({ idle: '空闲', preparing: '准备中', waiting_for_orders: '等待订单完成', activating: '正在启用', active: '已生效', failed: '失败' } as const)[value]
}
function policyStateLabel(value: 'in_band' | 'frozen' | 'flattening' | 'flatten_pending' | 'terminated') {
  return ({ in_band: '区间内', frozen: '已冻结', flattening: '正在平仓', flatten_pending: '等待平仓', terminated: '已终止' } as const)[value]
}
function riskOutcomeLabel(value: 'allowed' | 'capped' | 'daily_loss_terminated' | 'total_loss_terminated') {
  return ({ allowed: '允许', capped: '已限额', daily_loss_terminated: '日损失终止', total_loss_terminated: '总损失终止' } as const)[value]
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
  const [configurationRefreshVersion, setConfigurationRefreshVersion] = useState(0)
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
        setConfigurationRefreshVersion(version => version + 1)
        if (selectedIdRef.current) {
          setDetailRequestVersion(version => version + 1)
        }
      }
      current.onerror = () => disconnect(current)
      current.onclose = () => disconnect(current)
      current.onmessage = (event) => {
        try {
          const update = JSON.parse(event.data) as StreamEvent
          if (update.type === 'track_list_item_changed') {
            setTracks(previous => previous.map(item => item.id === update.track_id ? update.item : item))
            if (selectedIdRef.current === update.track_id) setConfigurationRefreshVersion(version => version + 1)
          }
          if (update.type === 'track_detail_changed') {
            setDetail(current => current?.identity.id === update.track_id ? update.detail : current)
            if (selectedIdRef.current === update.track_id) setConfigurationRefreshVersion(version => version + 1)
          }
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
    <section className="content">{selectedId ? detail ? <TrackPage key={detail.identity.id} detail={detail} configurationRefreshVersion={configurationRefreshVersion} /> : <h1>Track 载入中</h1> : <Overview readiness={readiness} account={account} tracks={tracks} onSelect={selectTrack} />}</section></div>
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

function TrackPage({ detail, configurationRefreshVersion }: { detail: TrackDetailView; configurationRefreshVersion: number }) {
  const [tab, setTab] = useState<'运行' | '配置' | '活动'>('运行')
  return <><div className="track-tabs" role="tablist" aria-label="Track 工作区">
    {(['运行', '配置', '活动'] as const).map(item => <button key={item} role="tab" aria-selected={tab === item} className={tab === item ? 'active' : ''} onClick={() => setTab(item)}>{item}</button>)}
  </div>
  {tab === '运行' && <TrackRunPanel detail={detail} />}
  {tab === '配置' && <ConfigurationPanel key={detail.identity.id} trackId={detail.identity.id} refreshVersion={configurationRefreshVersion} />}
  {tab === '活动' && <ActivityPanel activity={detail.activity} />}
  </>
}

function TrackRunPanel({ detail }: { detail: TrackDetailView }) {
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

function ActivityPanel({ activity }: { activity: TrackActivityItemView[] }) {
  return <article><h2>活动</h2>{activity.length ? <ul>{activity.map(item => <li key={`${item.ts}:${item.message}`}><strong>{title(item.level)}</strong><span>{item.ts} · {item.message}</span></li>)}</ul> : <p>当前没有活动记录。</p>}</article>
}

type Draft = TrackConfigurationActiveView

function cloneDraft(active: TrackConfigurationActiveView): Draft { return structuredClone(active) }
function inputValue(value: number) { return Number.isFinite(value) ? String(value) : '' }
type FlattenPolicy = Extract<BandProtectionPolicy, { flatten: unknown }>
function defaultFlattenPolicy(): FlattenPolicy { return { flatten: { trigger: { flatten_confirm: { bps: 500 } }, recover: { reentry_confirm: { bps: 500 } } } } }
function flattenPolicy(value: BandProtectionPolicy): FlattenPolicy { return typeof value === 'string' ? defaultFlattenPolicy() : value }
function triggerKind(trigger: BandFlattenTrigger) { return typeof trigger === 'string' ? trigger : 'flatten_confirm' }
function recoverKind(recover: BandRecoverPolicy) { return typeof recover === 'string' ? recover : 'reentry_confirm' }

const configurationFieldLabels: Record<string, string> = {
  lower_price: '下限价格', upper_price: '上限价格', long_exposure_units: '多头 exposure', short_exposure_units: '空头 exposure', notional_per_unit: '每单位名义金额', min_rebalance_units: '最小再平衡单位', shape_family: '形状族', max_notional: '最大名义金额', daily_loss_limit: '每日损失限额', total_loss_limit: '总损失限额',
  'risk_acquisition.initial_ratio': '初始比例', 'risk_acquisition.advantage_steps': '有利步数', 'risk_acquisition.min_release_steps': '最小释放步数', 'risk_acquisition.max_release_steps': '最大释放步数', 'risk_acquisition.catchup_ratio': '追赶比例', 'risk_acquisition.stale_release_minutes': '过期释放分钟',
  out_of_band_policy: '区间外策略', 'out_of_band_policy.flatten.trigger': 'Flatten 触发', 'out_of_band_policy.flatten.trigger.flatten_confirm.bps': 'Flatten 确认 bps', 'out_of_band_policy.flatten.recover': '恢复策略', 'out_of_band_policy.flatten.recover.reentry_confirm.bps': '重新进入确认 bps',
}
function changeValue(value: unknown) { return typeof value === 'number' ? format(value) : typeof value === 'string' ? title(value) : value === undefined ? '—' : String(value) }
function configurationChanges(active: Record<string, unknown>, draft: Record<string, unknown>, path = ''): { label: string; from: string; to: string }[] {
  const keys = new Set([...Object.keys(active), ...Object.keys(draft)])
  return [...keys].flatMap(key => {
    const nextPath = path ? `${path}.${key}` : key; const before = active[key]; const after = draft[key]
    if (before && after && typeof before === 'object' && typeof after === 'object' && !Array.isArray(before) && !Array.isArray(after)) return configurationChanges(before as Record<string, unknown>, after as Record<string, unknown>, nextPath)
    return JSON.stringify(before) === JSON.stringify(after) ? [] : [{ label: configurationFieldLabels[nextPath] ?? nextPath, from: changeValue(before), to: changeValue(after) }]
  })
}

function ConfigurationApplicationNotice({ application, status }: { application: TrackConfigurationApplicationView | null; status: TrackConfigurationApplicationStatus }) {
  if (status === 'idle') return null
  if (status === 'preparing') return <article className="application-notice"><h2>配置应用正在进行</h2><p>申请已被 server 接受，正在使用最新事实重新验证；配置尚未生效，不能再次编辑或提交。</p></article>
  if (status === 'waiting_for_orders') return <article className="application-notice"><h2>配置应用正在进行</h2><p>申请已被 server 接受，但尚未生效。正在等待旧订单终态和权威订单快照；页面不会重复提交、暂停、撤单或恢复执行。</p></article>
  if (status === 'activating') return <article className="application-notice"><h2>配置应用正在进行</h2><p>旧订单已经解决，正在安装已经验证的配置；配置尚未生效，不能再次编辑或提交。</p></article>
  if (status === 'active') return <article className="application-notice"><h2>配置已生效</h2><p>{application?.active_version ? `申请 ${application.application_id} 已完成。` : '当前配置已生效。'}</p></article>
  return <article className="application-notice failed"><h2>配置应用失败</h2><p>{application?.message ?? 'server 未能完成本次配置应用。当前生效值仍以 server 返回的配置快照为准；请重新载入、修改并预览后再提交。'}</p></article>
}

function ConfigurationPanel({ trackId, refreshVersion }: { trackId: string; refreshVersion: number }) {
  const [configuration, setConfiguration] = useState<TrackConfigurationResponse | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [loadError, setLoadError] = useState(false)
  const [preview, setPreview] = useState<TrackConfigurationPreviewResponse | null>(null)
  const [previewError, setPreviewError] = useState(false)
  const [previewing, setPreviewing] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const [applicationId, setApplicationId] = useState<string | null>(null)
  const [applyError, setApplyError] = useState<'network' | 'conflict' | null>(null)
  const [applying, setApplying] = useState(false)
  const previewController = useRef<AbortController | null>(null)
  const previewSequence = useRef(0)
  const applicationController = useRef<AbortController | null>(null)
  const applicationSequence = useRef(0)
  const configurationController = useRef<AbortController | null>(null)
  const configurationSequence = useRef(0)

  const invalidatePreview = () => { previewSequence.current += 1; previewController.current?.abort(); previewController.current = null; setPreview(null); setPreviewError(false); setPreviewing(false) }
  const invalidateApplication = () => { applicationSequence.current += 1; applicationController.current?.abort(); applicationController.current = null; setConfirming(false); setApplying(false); setApplyError(null); setApplicationId(null) }

  const load = () => {
    configurationController.current?.abort()
    const controller = new AbortController()
    configurationController.current = controller
    const request = configurationSequence.current + 1; configurationSequence.current = request
    setLoadError(false)
    get<TrackConfigurationResponse>(`/tracks/${encodeURIComponent(trackId)}/configuration`, { signal: controller.signal })
      .then(value => { if (!controller.signal.aborted && request === configurationSequence.current) { setConfiguration(value); setDraft(cloneDraft(value.active)); setPreview(null) } })
      .catch(() => { if (!controller.signal.aborted && request === configurationSequence.current) setLoadError(true) })
    return controller
  }
  useEffect(() => { const controller = load(); return () => { controller.abort(); configurationSequence.current += 1; invalidatePreview(); invalidateApplication() } }, [trackId, refreshVersion])
  const changed = configuration !== null && draft !== null && JSON.stringify(configuration.active) !== JSON.stringify(draft)
  const update = <K extends keyof Draft>(field: K, value: Draft[K]) => { setDraft(current => current ? { ...current, [field]: value } : current); invalidatePreview(); invalidateApplication() }
  const updateRisk = (field: keyof Draft['risk_acquisition'], value: number) => { setDraft(current => current ? { ...current, risk_acquisition: { ...current.risk_acquisition, [field]: value } } : current); invalidatePreview(); invalidateApplication() }
  const discard = () => { if (configuration) { setDraft(cloneDraft(configuration.active)); invalidatePreview(); invalidateApplication() } }
  const submitPreview = async () => {
    if (!draft) return
    previewController.current?.abort()
    const controller = new AbortController(); previewController.current = controller
    const request = previewSequence.current + 1; previewSequence.current = request
    setPreviewing(true); setPreviewError(false); setPreview(null)
    try {
      const value = await get<TrackConfigurationPreviewResponse>(`/tracks/${encodeURIComponent(trackId)}/configuration/preview`, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ candidate: draft }), signal: controller.signal })
      if (!controller.signal.aborted && request === previewSequence.current) setPreview(value)
    } catch { if (!controller.signal.aborted && request === previewSequence.current) setPreviewError(true) } finally { if (request === previewSequence.current) setPreviewing(false) }
  }
  const applicationStatus: TrackConfigurationApplicationStatus = configuration?.application?.status ?? configuration?.application_status ?? 'idle'
  const applicationInProgress = applicationStatus === 'preparing' || applicationStatus === 'waiting_for_orders' || applicationStatus === 'activating'
  const apply = async () => {
    if (!draft || !changed || !preview?.valid || applicationInProgress) return
    const id = applicationId ?? crypto.randomUUID()
    if (!applicationId) setApplicationId(id)
    applicationController.current?.abort()
    const controller = new AbortController(); applicationController.current = controller
    const request = applicationSequence.current + 1; applicationSequence.current = request
    setApplying(true); setApplyError(null)
    try {
      await get(`/tracks/${encodeURIComponent(trackId)}/configuration`, { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ application_id: id, expected_version: configuration!.version, candidate: draft }), signal: controller.signal })
      if (!controller.signal.aborted && request === applicationSequence.current) { invalidatePreview(); setConfirming(false); setApplicationId(null); load() }
    } catch (error) {
      if (!controller.signal.aborted && request === applicationSequence.current) setApplyError(error instanceof Error && error.message.includes(' 409') ? 'conflict' : 'network')
    } finally { if (request === applicationSequence.current) setApplying(false) }
  }
  if (loadError) return <article><h2>无法读取配置</h2><button onClick={() => load()}>重试载入配置</button></article>
  if (!configuration || !draft) return <h1>配置载入中</h1>
  const active = configuration.active
  const policyKind = typeof draft.out_of_band_policy === 'string' ? draft.out_of_band_policy : 'flatten'
  const setPolicy = (value: string) => update('out_of_band_policy', value === 'flatten' ? flattenPolicy(draft.out_of_band_policy) : value as 'freeze' | 'terminate')
  const setFlattenTrigger = (value: string) => { const policy = flattenPolicy(draft.out_of_band_policy); update('out_of_band_policy', { flatten: { ...policy.flatten, trigger: value === 'immediate' ? 'immediate' : { flatten_confirm: { bps: 500 } } } }) }
  const setFlattenRecover = (value: string) => { const policy = flattenPolicy(draft.out_of_band_policy); update('out_of_band_policy', { flatten: { ...policy.flatten, recover: value === 'back_in_band' ? 'back_in_band' : { reentry_confirm: { bps: 500 } } } }) }
  const setFlattenBps = (part: 'trigger' | 'recover', value: number) => { const policy = flattenPolicy(draft.out_of_band_policy); const next = part === 'trigger' ? { ...policy.flatten, trigger: { flatten_confirm: { bps: value } } } : { ...policy.flatten, recover: { reentry_confirm: { bps: value } } }; update('out_of_band_policy', { flatten: next }) }
  const numberInput = (label: string, field: Exclude<keyof Draft, 'risk_acquisition' | 'out_of_band_policy' | 'shape_family'>) => <label>{label}<input aria-label={label} type="number" value={inputValue(draft[field] as number)} onChange={event => update(field, Number(event.target.value))} /></label>
  const riskInput = (label: string, field: keyof Draft['risk_acquisition']) => <label>{label}<input aria-label={label} type="number" value={inputValue(draft.risk_acquisition[field])} onChange={event => updateRisk(field, Number(event.target.value))} /></label>
  const flatten = policyKind === 'flatten' ? flattenPolicy(draft.out_of_band_policy).flatten : null
  const changes = configurationChanges(active as unknown as Record<string, unknown>, draft as unknown as Record<string, unknown>)
  return <div className="configuration"><p className="eyebrow">配置应用状态 · {applicationStatusLabel(applicationStatus)}{configuration.application?.message ? ` · ${configuration.application.message}` : ''}</p>
    <ConfigurationApplicationNotice application={configuration.application} status={applicationStatus} />
    {changed && <p className="unsaved">有未保存修改</p>}
    <div className="two-column"><article><h2>当前生效值</h2><dl><div><dt>价格区间</dt><dd>{format(active.lower_price, 2)} – {format(active.upper_price, 2)}</dd></div><div><dt>多/空 exposure</dt><dd>{format(active.long_exposure_units)} / {format(active.short_exposure_units)}</dd></div><div><dt>每单位名义金额</dt><dd>{format(active.notional_per_unit)}</dd></div><div><dt>区间外策略</dt><dd>{policy(active.out_of_band_policy)}</dd></div><div><dt>最大名义金额</dt><dd>{format(active.max_notional)}</dd></div></dl></article><article><h2>只读身份与重启字段</h2><dl><div><dt>Track ID</dt><dd>{configuration.identity.id}</dd></div><div><dt>Venue</dt><dd>{configuration.identity.instrument.venue}</dd></div><div><dt>Symbol</dt><dd>{configuration.identity.instrument.symbol}</dd></div><div><dt>杠杆</dt><dd>{configuration.restart_required.leverage}</dd></div><div><dt>Tick 超时（秒）</dt><dd>{configuration.restart_required.tick_timeout_secs}</dd></div></dl></article></div>
    <article><h2>可编辑草稿</h2><fieldset className="configuration-editor" disabled={applicationInProgress}><div className="config-fields">{numberInput('下限价格', 'lower_price')}{numberInput('上限价格', 'upper_price')}{numberInput('多头 exposure', 'long_exposure_units')}{numberInput('空头 exposure', 'short_exposure_units')}{numberInput('每单位名义金额', 'notional_per_unit')}{numberInput('最小再平衡单位', 'min_rebalance_units')}<label>形状族<select aria-label="形状族" value={draft.shape_family} onChange={event => update('shape_family', event.target.value as Draft['shape_family'])}><option value="linear">linear</option><option value="inertial">inertial</option><option value="responsive">responsive</option></select></label><label>区间外策略<select aria-label="区间外策略" value={policyKind} onChange={event => setPolicy(event.target.value)}><option value="freeze">freeze</option><option value="terminate">terminate</option><option value="flatten">flatten</option></select></label>{flatten && <><label>Flatten 触发<select aria-label="Flatten 触发" value={triggerKind(flatten.trigger)} onChange={event => setFlattenTrigger(event.target.value)}><option value="immediate">immediate</option><option value="flatten_confirm">flatten_confirm</option></select></label>{typeof flatten.trigger !== 'string' && <label>Flatten 确认 bps<input aria-label="Flatten 确认 bps" type="number" value={inputValue(flatten.trigger.flatten_confirm.bps)} onChange={event => setFlattenBps('trigger', Number(event.target.value))} /></label>}<label>恢复策略<select aria-label="恢复策略" value={recoverKind(flatten.recover)} onChange={event => setFlattenRecover(event.target.value)}><option value="back_in_band">back_in_band</option><option value="reentry_confirm">reentry_confirm</option></select></label>{typeof flatten.recover !== 'string' && <label>重新进入确认 bps<input aria-label="重新进入确认 bps" type="number" value={inputValue(flatten.recover.reentry_confirm.bps)} onChange={event => setFlattenBps('recover', Number(event.target.value))} /></label>}</>}{numberInput('最大名义金额', 'max_notional')}{numberInput('每日损失限额', 'daily_loss_limit')}{numberInput('总损失限额', 'total_loss_limit')}{riskInput('初始比例', 'initial_ratio')}{riskInput('有利步数', 'advantage_steps')}{riskInput('最小释放步数', 'min_release_steps')}{riskInput('最大释放步数', 'max_release_steps')}{riskInput('追赶比例', 'catchup_ratio')}{riskInput('过期释放分钟', 'stale_release_minutes')}</div><div className="config-actions"><button onClick={discard} disabled={!changed}>放弃修改</button><button onClick={() => void submitPreview()} disabled={previewing}>{previewing ? '预览中' : '重新预览'}</button><button onClick={() => setConfirming(true)} disabled={!changed || !preview?.valid || applying}>保存并应用</button></div></fieldset></article>
    {confirming && <div role="dialog" aria-modal="true" aria-label="确认保存并应用" className="confirmation"><h2>确认保存并应用</h2><p>这会更新实例配置，并可能产生真实订单。</p><dl>{changes.map(change => <div key={change.label}><dt>{change.label}</dt><dd>{change.from} → {change.to}</dd></div>)}</dl><button onClick={() => setConfirming(false)} disabled={applying}>取消</button><button onClick={() => void apply()} disabled={applying}>{applying ? '正在提交' : '确认保存并应用'}</button></div>}
    {applyError === 'conflict' && <div role="alert" className="alert"><strong>配置版本冲突</strong><span>配置已变化，请重新加载后再试。</span><button onClick={() => load()}>重新加载</button></div>}{applyError === 'network' && <div role="alert" className="alert"><strong>提交结果未确认</strong><span>可使用同一申请 ID 重试。</span><button onClick={() => void apply()}>重试提交</button></div>}
    {previewError && <div role="alert" className="alert"><strong>预览失败</strong><button onClick={() => void submitPreview()}>重试预览</button></div>}{preview && <PreviewResult preview={preview} symbol={configuration.identity.instrument.symbol} />}
  </div>
}

function PreviewResult({ preview, symbol }: { preview: TrackConfigurationPreviewResponse; symbol: string }) {
  const impact = preview.impact
  if (!impact) return <article className="preview"><h2>预览结果</h2><p>预览时间 {preview.previewed_at} · {preview.valid ? '有效' : '不可应用'}</p><PreviewMessages preview={preview} /></article>
  const quantityUnit = impact.position_quantity_unit === 'base_asset' ? `基础资产（${symbol}）` : impact.position_quantity_unit === 'contracts' ? '合约' : '—'
  return <article className="preview"><h2>预览结果</h2><p>预览时间 {preview.previewed_at} · {preview.valid ? '有效' : '不可应用'}</p><dl><div><dt>实际数量不变</dt><dd>{format(impact.position_quantity)} {quantityUnit}</dd></div><div><dt>当前 exposure（旧配置单位）</dt><dd>{format(impact.current_exposure)}</dd></div><div><dt>候选 exposure（候选配置单位）</dt><dd>{format(impact.candidate_current_exposure)}</dd></div><div><dt>当前 committed 策略目标</dt><dd>{format(impact.active_policy_target)}</dd></div><div><dt>候选曲线目标</dt><dd>{format(impact.candidate_curve_target)}</dd></div><div><dt>候选策略目标 / 状态</dt><dd>{format(impact.candidate_policy_target)} / {policyStateLabel(impact.candidate_policy_state)}</dd></div><div><dt>候选风险批准目标 / 结果</dt><dd>{format(impact.candidate_risk_approved_target)} / {riskOutcomeLabel(impact.candidate_risk_outcome)}</dd></div><div><dt>策略目标数量（当前 → 候选）</dt><dd>{format(impact.active_policy_target_position_qty)} → {format(impact.candidate_policy_target_position_qty)}</dd></div><div><dt>目标数量变化</dt><dd>{format(impact.target_position_qty_delta)}</dd></div><div><dt>策略目标 signed notional（当前 → 候选）</dt><dd>{format(impact.active_policy_target_signed_notional)} → {format(impact.candidate_policy_target_signed_notional)}</dd></div><div><dt>目标 signed notional 变化</dt><dd>{format(impact.target_signed_notional_delta)} {impact.notional_asset}</dd></div><div><dt>候选目标相对当前 exposure</dt><dd>{format(impact.candidate_target_delta_from_current_exposure)}</dd></div><div><dt>候选数量相对当前</dt><dd>{format(impact.candidate_position_qty_delta_from_current)}</dd></div><div><dt>每 exposure 单位对应数量</dt><dd>{format(impact.position_qty_per_unit)} {quantityUnit}</dd></div><div><dt>场所最小调整数量</dt><dd>{format(impact.minimum_rebalance_position_qty)} {quantityUnit}</dd></div><div><dt>归一化最小调整数量</dt><dd>{format(impact.normalized_minimum_rebalance_position_qty)} {quantityUnit}</dd></div><div><dt>最小调整名义金额</dt><dd>{format(impact.minimum_rebalance_notional)} {impact.notional_asset}</dd></div><div><dt>最大目标数量</dt><dd>{format(impact.maximum_target_position_qty)} {quantityUnit}</dd></div><div><dt>初始 frontier</dt><dd>{format(impact.initial_frontier)}</dd></div><div><dt>初始执行目标</dt><dd>{format(impact.initial_execution_target)}</dd></div></dl>{preview.capacity_proof && <section><h3>容量证明</h3><dl><div><dt>可增加名义金额</dt><dd>{format(preview.capacity_proof.available_increase_notional)} {preview.capacity_proof.asset}</dd></div><div><dt>所需增加名义金额</dt><dd>{format(preview.capacity_proof.required_increase_notional)} {preview.capacity_proof.asset}</dd></div><div><dt>观测时间</dt><dd>{preview.capacity_proof.observed_at}</dd></div></dl></section>}<PreviewMessages preview={preview} /></article>
}

function PreviewMessages({ preview }: { preview: TrackConfigurationPreviewResponse }) { return <>{preview.field_errors.length > 0 && <section><h3>字段错误</h3><ul>{preview.field_errors.map(item => <li key={`${item.field}:${item.message}`}><strong>{item.field}</strong><span>{item.message}</span></li>)}</ul></section>}{preview.blockers.length > 0 && <section><h3>阻止项</h3><ul>{preview.blockers.map(item => <li key={`${item.code}:${item.message}`}><strong>{item.code}</strong><span>{item.message}</span></li>)}</ul></section>}</> }
