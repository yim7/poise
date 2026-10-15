import { useEffect, useRef, useState } from 'react'
import { HttpResponseError, requestJson } from './api'
import type { TrackConfigurationActiveView, TrackCreationCandidate, TrackCreationContext, TrackCreationPreviewResponse } from './types'

function emptyCandidate(): TrackCreationCandidate {
  return {
    track_id: '', symbol: '', leverage: NaN, tick_timeout_secs: 30,
    configuration: {
      lower_price: NaN, upper_price: NaN, long_exposure_units: NaN, short_exposure_units: 0,
      notional_per_unit: NaN, max_notional: NaN, daily_loss_limit: NaN, total_loss_limit: NaN,
      min_rebalance_units: 0.5, shape_family: 'linear', out_of_band_policy: 'freeze',
      risk_acquisition: { initial_ratio: 0.5, advantage_steps: 2, min_release_steps: 1, max_release_steps: 4, catchup_ratio: 0.25, stale_release_minutes: 60 },
    },
  }
}
function number(value: number) { return Number.isFinite(value) ? String(value) : '' }
function format(value: number) { return value.toLocaleString('zh-CN', { minimumFractionDigits: 4, maximumFractionDigits: 4 }) }
function sameCandidate(left: TrackCreationCandidate, right: TrackCreationCandidate) {
  const normalize = (value: unknown): unknown => value !== null && typeof value === 'object'
    ? Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)).map(([key, item]) => [key, normalize(item)]))
    : value
  return JSON.stringify(normalize(left)) === JSON.stringify(normalize(right))
}
type NumericConfigurationField = Exclude<keyof TrackConfigurationActiveView, 'shape_family' | 'out_of_band_policy' | 'risk_acquisition'>

export function TrackCreationPanel({ exchange, deployment, onSaved }: { exchange: string | null; deployment: string | null; onSaved: (context: TrackCreationContext) => void }) {
  const [context, setContext] = useState<TrackCreationContext | null>(null)
  const [candidate, setCandidate] = useState(emptyCandidate)
  const [preview, setPreview] = useState<TrackCreationPreviewResponse | null>(null)
  const [previewing, setPreviewing] = useState(false)
  const [saving, setSaving] = useState(false)
  const [confirming, setConfirming] = useState(false)
  const [saved, setSaved] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const sequence = useRef(0)
  const previewController = useRef<AbortController | null>(null)
  const mounted = useRef(true)
  const contextController = useRef<AbortController | null>(null)

  const load = async () => {
    contextController.current?.abort()
    const controller = new AbortController(); contextController.current = controller
    setLoading(true)
    try {
      const value = await requestJson<TrackCreationContext>('/track-creation', { signal: controller.signal })
      if (!controller.signal.aborted) setContext(value)
    } catch (cause) {
      if (!controller.signal.aborted) { setContext(null); setError(cause instanceof HttpResponseError ? cause.message : '无法读取实例配置，请确认服务端已更新后重试。') }
    } finally { if (!controller.signal.aborted) setLoading(false) }
  }
  useEffect(() => {
    mounted.current = true; void load()
    return () => { mounted.current = false; sequence.current += 1; previewController.current?.abort(); contextController.current?.abort() }
  }, [])
  const invalidate = () => {
    sequence.current += 1; previewController.current?.abort(); setPreview(null); setPreviewing(false); setConfirming(false); setError(null)
  }
  const update = (next: TrackCreationCandidate) => { setCandidate(next); invalidate() }
  const configuration = candidate.configuration
  const numericInput = (label: string, field: NumericConfigurationField, min = 0) => <label>{label}<input aria-label={label} type="number" step="any" min={min} required value={number(configuration[field])} onChange={event => update({ ...candidate, configuration: { ...configuration, [field]: event.target.value === '' ? NaN : Number(event.target.value) } })} /></label>
  const riskInput = (label: string, field: keyof TrackConfigurationActiveView['risk_acquisition']) => <label>{label}<input aria-label={label} type="number" step="any" required value={number(configuration.risk_acquisition[field])} onChange={event => update({ ...candidate, configuration: { ...configuration, risk_acquisition: { ...configuration.risk_acquisition, [field]: event.target.value === '' ? NaN : Number(event.target.value) } } })} /></label>
  const submitPreview = async () => {
    if (!context) return
    invalidate(); setPreviewing(true)
    const current = sequence.current; const controller = new AbortController(); previewController.current = controller
    try {
      const value = await requestJson<TrackCreationPreviewResponse>('/track-creation/preview', {
        method: 'POST', headers: { 'content-type': 'application/json' }, signal: controller.signal,
        body: JSON.stringify({ expected_version: context.version, candidate }),
      })
      if (!controller.signal.aborted && current === sequence.current) setPreview(value)
    } catch (cause) {
      if (!controller.signal.aborted && current === sequence.current) {
        setError(cause instanceof Error ? cause.message : '无法预览接管影响')
        if (cause instanceof HttpResponseError && cause.status === 409) void load()
      }
    } finally { if (!controller.signal.aborted && current === sequence.current) setPreviewing(false) }
  }
  const complete = (receipt: TrackCreationContext) => {
    onSaved(receipt)
    if (mounted.current) { setContext(receipt); setSaved(true); setConfirming(false); setError(null) }
  }
  const save = async () => {
    if (!context || !preview || saving) return
    setSaving(true); setError(null)
    try {
      complete(await requestJson<TrackCreationContext>('/tracks', {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ expected_version: context.version, preview_token: preview.preview_token, candidate }),
      }))
    } catch (cause) {
      if (!mounted.current) return
      setConfirming(false); setPreview(null)
      if (cause instanceof HttpResponseError) {
        setError(cause.message)
        if (cause.status === 409) void load()
      } else {
        // A network failure may follow a successful write. Re-read durable state;
        // never automatically repeat a mutation with an uncertain outcome.
        try {
          const receipt = await requestJson<TrackCreationContext>('/track-creation')
          if (receipt.pending.some(item => sameCandidate(item, candidate))) complete(receipt)
          else { setContext(receipt); setError('保存结果未确认，请检查待重启列表后重新预览。') }
        } catch { setError('保存结果未确认，请重新载入待重启列表后核对。') }
      }
    } finally { if (mounted.current) setSaving(false) }
  }

  if (saved) return <div className="track-creation"><p className="eyebrow">新增 Track</p><h1>已保存，等待重启</h1><article role="status"><h2>{candidate.track_id} · {candidate.symbol}</h2><p>配置已写入。当前进程尚未接管此合约；重启实例后会读取当时的真实仓位、撤销该合约挂单并按策略管理。</p><p>重启也会重新初始化已有 Tracks。请在重启后检查仓位、目标和交易就绪状态。</p><div className="config-actions"><button onClick={() => { setCandidate(emptyCandidate()); setSaved(false); invalidate(); void load() }}>继续新增</button></div></article></div>
  return <div className="track-creation configuration">
    <p className="eyebrow">{exchange ?? '交易所载入中'} · {deployment === 'mainnet' ? 'MAINNET 主网' : deployment ?? '环境载入中'}</p><h1>新增 Track</h1>
    <p className="creation-intro">配置保存后等待重启。接管该账户、该合约的整个净仓位，并依据价格区间调整持仓。</p>
    {error && <div className="alert" role="alert"><span>{error}</span>{!context && <button onClick={() => void load()}>重新载入配置</button>}</div>}
    {context?.pending?.length ? <article className="pending-tracks"><h2>已保存，待重启</h2><ul>{context.pending.map(item => <li key={item.track_id}>{item.track_id} · {item.symbol}</li>)}</ul></article> : null}
    <div className="configuration-workspace">
      <section className="configuration-form" aria-label="新增 Track 配置"><form onSubmit={event => { event.preventDefault(); void submitPreview() }}><article>
        <h2>合约与策略</h2><p>{exchange === 'hyperliquid' ? '使用交易所完整名称，例如 BTC 或 xyz:GOLD。' : '使用交易所合约名称，与已有仓位保持一致。'}</p>
        <fieldset className="configuration-editor" disabled={saving || loading || !context}><div className="config-fields">
          <label>Track ID<input aria-label="Track ID" required value={candidate.track_id} onChange={event => update({ ...candidate, track_id: event.target.value })} placeholder="例如 gold" /></label>
          <label>合约名称<input aria-label="合约名称" required value={candidate.symbol} onChange={event => update({ ...candidate, symbol: event.target.value })} placeholder={exchange === 'hyperliquid' ? 'xyz:GOLD' : '交易所合约标识'} /></label>
          <label>杠杆<input aria-label="杠杆" type="number" min="1" step="1" required value={number(candidate.leverage)} onChange={event => update({ ...candidate, leverage: event.target.value === '' ? NaN : Number(event.target.value) })} /></label>
          {numericInput('下限价格', 'lower_price')}{numericInput('上限价格', 'upper_price')}{numericInput('多头 exposure', 'long_exposure_units')}{numericInput('空头 exposure', 'short_exposure_units')}{numericInput('每单位名义金额', 'notional_per_unit')}{numericInput('最大名义金额', 'max_notional')}{numericInput('每日损失限额', 'daily_loss_limit')}{numericInput('总损失限额', 'total_loss_limit')}
        </div><p className="creation-hint">空头 exposure 设为 0 表示只做多，仍可能卖出已有多仓。金额单位由合约 metadata 确定，预览后显示。</p>
        <details className="creation-advanced"><summary>执行与风险获取参数</summary><div className="config-fields">
          {numericInput('最小再平衡单位', 'min_rebalance_units')}
          <label>Tick 超时（秒）<input aria-label="Tick 超时（秒）" type="number" min="1" step="1" required value={number(candidate.tick_timeout_secs)} onChange={event => update({ ...candidate, tick_timeout_secs: Number(event.target.value) })} /></label>
          <label>形状族<select value={configuration.shape_family} onChange={event => update({ ...candidate, configuration: { ...configuration, shape_family: event.target.value as TrackConfigurationActiveView['shape_family'] } })}><option value="linear">linear</option><option value="inertial">inertial</option><option value="responsive">responsive</option></select></label>
          <label>区间外策略<select value={typeof configuration.out_of_band_policy === 'string' ? configuration.out_of_band_policy : 'flatten'} onChange={event => update({ ...candidate, configuration: { ...configuration, out_of_band_policy: event.target.value === 'flatten' ? { flatten: { trigger: { flatten_confirm: { bps: 500 } }, recover: { reentry_confirm: { bps: 500 } } } } : event.target.value as 'freeze' | 'terminate' } })}><option value="freeze">冻结目标</option><option value="flatten">平仓（500 bps 确认）</option><option value="terminate">终止并平仓</option></select></label>
          {riskInput('初始比例', 'initial_ratio')}{riskInput('有利步数', 'advantage_steps')}{riskInput('最小释放步数', 'min_release_steps')}{riskInput('最大释放步数', 'max_release_steps')}{riskInput('追赶比例', 'catchup_ratio')}{riskInput('过期释放分钟', 'stale_release_minutes')}
        </div></details>
        <div className="config-actions"><button type="submit" disabled={previewing}>{previewing ? '预览中' : '预览接管影响'}</button><button type="button" className="primary" disabled={!preview || previewing || saving} onClick={() => setConfirming(true)}>保存，重启后生效</button></div></fieldset>
      </article></form></section>
      <aside className="configuration-context" aria-label="接管预览">{preview ? <article><h2>接管影响</h2><p>快照 {preview.previewed_at}</p><dl>
        <div><dt>已有净仓位</dt><dd>{format(preview.position_quantity)} {preview.quantity_unit}</dd></div><div><dt>当前 Mark</dt><dd>{format(preview.mark_price)}</dd></div>
        <div><dt>策略目标数量</dt><dd>{format(preview.policy_target_quantity)} {preview.quantity_unit}</dd></div><div><dt>首次执行目标估算</dt><dd>{format(preview.initial_execution_quantity)} {preview.quantity_unit}</dd></div>
        <div><dt>所需 / 可增加名义金额</dt><dd>{format(preview.required_increase_notional)} / {format(preview.available_increase_notional)} {preview.notional_asset}</dd></div><div><dt>亏损限额资产</dt><dd>{preview.settlement_asset}</dd></div>
      </dl>{Math.abs(preview.initial_execution_quantity) < Math.abs(preview.position_quantity) && <p className="creation-reduction">接管后可能减仓：首次执行目标低于当前持仓。</p>}<h3>重启时将撤销的挂单 · {preview.orders_to_cancel_on_restart.length}</h3>{preview.orders_to_cancel_on_restart.length ? <ul className="creation-orders">{preview.orders_to_cancel_on_restart.map(order => <li key={order.order_id}><strong>{order.order_id}</strong><span>{order.side === 'buy' ? '买' : '卖'} {format(order.quantity)} {preview.quantity_unit} @ {format(order.price)}</span></li>)}</ul> : <p>此快照没有挂单。</p>}
        <p className="creation-hint">这里只读估算当前快照。重启时会重新读取仓位、行情和 PNL，实际目标可能变化；增加风险仍受获取条件限制。</p>
      </article> : <article className="preview-empty"><h2>接管影响</h2><p>填写参数并预览，核对已有仓位、目标和会撤销的挂单，再确认保存。</p></article>}</aside>
    </div>
    {confirming && preview && <div className="confirmation" role="dialog" aria-modal="true" aria-label="确认新增 Track"><h2>确认新增 Track</h2><p>{candidate.track_id} · {candidate.symbol} · {deployment === 'mainnet' ? 'MAINNET 主网' : deployment}</p><p>配置保存后等待重启；重启时会撤销此合约所有挂单（当前快照 {preview.orders_to_cancel_on_restart.length} 笔），并接管整个净仓位。</p><p>已有仓位 {format(preview.position_quantity)} {preview.quantity_unit} → 首次执行目标估算 {format(preview.initial_execution_quantity)} {preview.quantity_unit}。</p><p>启动会将此合约杠杆设为 {candidate.leverage} 倍，并重新初始化已有 Tracks。</p><button disabled={saving} onClick={() => setConfirming(false)}>取消</button><button disabled={saving} onClick={() => void save()}>{saving ? '正在保存' : '确认保存'}</button></div>}
  </div>
}
