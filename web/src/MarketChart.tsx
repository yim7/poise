import { useEffect, useMemo, useState } from 'react'
import type { CandleIntervalView, ExecutionBindingView, TrackChartResponse, TrackDetailView } from './types'

const intervals: { value: CandleIntervalView; label: string }[] = [
  { value: 'one_minute', label: '1m' },
  { value: 'five_minutes', label: '5m' },
  { value: 'fifteen_minutes', label: '15m' },
  { value: 'one_hour', label: '1h' },
]

const intervalMillis: Record<CandleIntervalView, number> = {
  one_minute: 60_000,
  five_minutes: 5 * 60_000,
  fifteen_minutes: 15 * 60_000,
  one_hour: 60 * 60_000,
}

const exchangeOrderStatuses = new Set(['working', 'cancel_pending'])
const pendingOrderStatuses = new Set(['submit_pending', 'submit_authorized', 'cancel_after_submit'])

function number(value: number, digits = 2) {
  return value.toLocaleString('zh-CN', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

function sideLabel(side: string) {
  return side === 'buy' ? '买' : side === 'sell' ? '卖' : side
}

type PriceLine = {
  id: string
  price: number
  label: string
  detail: string
  kind: 'order' | 'pending' | 'trigger' | 'range' | 'current'
}

function bindingLine(binding: ExecutionBindingView): PriceLine | null {
  if (!binding.order) return null
  const kind = exchangeOrderStatuses.has(binding.status)
    ? 'order'
    : pendingOrderStatuses.has(binding.status)
      ? 'pending'
      : null
  if (!kind) return null
  return {
    id: `binding:${binding.id}`,
    price: binding.order.price,
    label: kind === 'order' ? '交易所挂单' : '提交中订单',
    detail: `${sideLabel(binding.order.side)} ${number(binding.order.quantity, 4)} @ ${number(binding.order.price)}`,
    kind,
  }
}

function priceLines(detail: TrackDetailView): PriceLine[] {
  const lines = detail.execution.bindings.map(bindingLine).filter((line): line is PriceLine => line !== null)
  if (detail.execution.risk_acquisition?.next_advantage_price != null) {
    lines.push({
      id: 'next-advantage',
      price: detail.execution.risk_acquisition.next_advantage_price,
      label: '潜在挂单条件',
      detail: `下一次风险释放 @ ${number(detail.execution.risk_acquisition.next_advantage_price)}`,
      kind: 'trigger',
    })
  }
  lines.push(
    { id: 'lower', price: detail.strategy.lower_price, label: '策略下界', detail: number(detail.strategy.lower_price), kind: 'range' },
    { id: 'upper', price: detail.strategy.upper_price, label: '策略上界', detail: number(detail.strategy.upper_price), kind: 'range' },
  )
  if (detail.status.strategy_price != null) {
    lines.push({ id: 'current', price: detail.status.strategy_price, label: '策略价', detail: number(detail.status.strategy_price), kind: 'current' })
  }
  return lines
}

async function loadChart(trackId: string, interval: CandleIntervalView, signal: AbortSignal) {
  const response = await fetch(`/tracks/${encodeURIComponent(trackId)}/chart?interval=${interval}&limit=100`, { signal })
  if (!response.ok) throw new Error(`chart ${response.status}`)
  const chart = await response.json() as TrackChartResponse
  if (!Array.isArray(chart.candles)) throw new Error('chart response is missing candles')
  return chart
}

function withLivePrice(chart: TrackChartResponse, price: number | null) {
  if (price == null || !Number.isFinite(price) || price <= 0) return chart
  const candles = [...chart.candles]
  const duration = intervalMillis[chart.interval]
  const openTime = Math.floor(Date.now() / duration) * duration
  const last = candles.at(-1)
  if (last && new Date(last.open_time).getTime() === openTime) {
    candles[candles.length - 1] = { ...last, high: Math.max(last.high, price), low: Math.min(last.low, price), close: price }
  } else {
    candles.push({ open_time: new Date(openTime).toISOString(), open: price, high: price, low: price, close: price })
    if (candles.length > 100) candles.shift()
  }
  return { ...chart, candles }
}

export function MarketChart({ detail }: { detail: TrackDetailView }) {
  const [interval, setIntervalValue] = useState<CandleIntervalView>('five_minutes')
  const [chart, setChart] = useState<TrackChartResponse | null>(null)
  const [error, setError] = useState(false)
  const [fullRange, setFullRange] = useState(false)

  useEffect(() => {
    let stopped = false
    let controller: AbortController | null = null
    const refresh = async () => {
      controller?.abort()
      controller = new AbortController()
      try {
        const next = await loadChart(detail.identity.id, interval, controller.signal)
        if (!stopped) { setChart(next); setError(false) }
      } catch (requestError) {
        if (!stopped && !(requestError instanceof DOMException && requestError.name === 'AbortError')) setError(true)
      }
    }
    setChart(null)
    setError(false)
    void refresh()
    const timer = window.setInterval(() => void refresh(), 15_000)
    return () => { stopped = true; controller?.abort(); window.clearInterval(timer) }
  }, [detail.identity.id, interval])

  const lines = useMemo(() => priceLines(detail), [detail])
  const liveChart = useMemo(() => chart ? withLivePrice(chart, detail.market.mark_price ?? detail.status.strategy_price) : null, [chart, detail.market.mark_price, detail.status.strategy_price])
  const orders = lines.filter(line => line.kind === 'order' || line.kind === 'pending')
  const triggers = lines.filter(line => line.kind === 'trigger')
  return <article className="market-chart-panel">
    <header className="chart-heading"><div><h2>行情与订单</h2><p>{error ? '行情同步中断 · 当前显示最近数据' : '价格走势 / 执行位置'}</p></div><div className="intervals" role="group" aria-label="K线周期">{intervals.map(item => <button key={item.value} aria-pressed={interval === item.value} className={interval === item.value ? 'active' : ''} onClick={() => setIntervalValue(item.value)}>{item.label}</button>)}<button aria-pressed={fullRange} className={fullRange ? 'active' : ''} onClick={() => setFullRange(value => !value)}>完整区间</button></div></header>
    <div className="chart-layout">
      <div className="chart-canvas">{liveChart?.candles.length ? <CandleChart candles={liveChart.candles} lines={lines} fullRange={fullRange} /> : <div className="chart-empty">{error ? 'K 线暂时不可用，订单和触发位仍以右侧状态为准。' : '正在读取交易所 K 线…'}</div>}</div>
      <div className="price-line-list" aria-label="图上价格线">
        <section><h3>订单</h3>{orders.length ? orders.map(line => <div key={line.id} className={`price-line ${line.kind}`}><span>{line.label}</span><strong>{line.detail}</strong></div>) : <p>当前没有订单。</p>}</section>
        <section><h3>潜在位置</h3>{triggers.length ? triggers.map(line => <div key={line.id} className="price-line trigger"><span>{line.label}</span><strong>{line.detail}</strong></div>) : <p>当前没有下一触发价。</p>}</section>
        <section className="range-summary"><h3>范围</h3><span>{number(detail.strategy.lower_price)} – {number(detail.strategy.upper_price)}</span></section>
      </div>
    </div>
  </article>
}

function CandleChart({ candles, lines, fullRange }: { candles: TrackChartResponse['candles']; lines: PriceLine[]; fullRange: boolean }) {
  const width = 1000
  const height = 360
  const margin = { top: 14, right: 10, bottom: 28, left: 12 }
  const plotWidth = width - margin.left - margin.right
  const plotHeight = height - margin.top - margin.bottom
  const prices = [...candles.flatMap(candle => [candle.high, candle.low]), ...lines.filter(line => fullRange || line.kind !== 'range').map(line => line.price)]
  const rawMin = Math.min(...prices)
  const rawMax = Math.max(...prices)
  const padding = Math.max((rawMax - rawMin) * 0.04, rawMax * 0.0005)
  const min = rawMin - padding
  const max = rawMax + padding
  const y = (price: number) => margin.top + (max - price) / (max - min) * plotHeight
  const step = plotWidth / Math.max(candles.length, 1)
  const candleWidth = Math.max(2, Math.min(8, step * .62))
  const gridPrices = Array.from({ length: 5 }, (_, index) => max - (max - min) * index / 4)
  const timeIndexes = [0, Math.floor((candles.length - 1) / 2), candles.length - 1].filter((value, index, all) => all.indexOf(value) === index)

  return <svg viewBox={`0 0 ${width} ${height}`} role="img" aria-label="实时K线与订单价格线" preserveAspectRatio="none">
    {gridPrices.map(price => <g key={price}><line className="chart-grid-line" x1={margin.left} x2={width} y1={y(price)} y2={y(price)} /><text className="chart-axis-label" x={width - 6} y={y(price) - 4} textAnchor="end">{number(price)}</text></g>)}
    {lines.filter(line => line.price >= min && line.price <= max).map(line => <g key={line.id} className={`overlay-line ${line.kind}`}><title>{line.label} {line.detail}</title><line x1={margin.left} x2={width} y1={y(line.price)} y2={y(line.price)} /></g>)}
    {candles.map((candle, index) => {
      const x = margin.left + step * (index + .5)
      const up = candle.close >= candle.open
      const bodyY = Math.min(y(candle.open), y(candle.close))
      const bodyHeight = Math.max(1.5, Math.abs(y(candle.open) - y(candle.close)))
      return <g key={`${candle.open_time}:${index}`} className={up ? 'candle up' : 'candle down'}><line x1={x} x2={x} y1={y(candle.high)} y2={y(candle.low)} /><rect x={x - candleWidth / 2} y={bodyY} width={candleWidth} height={bodyHeight} /></g>
    })}
    {timeIndexes.map(index => <text key={index} className="chart-time-label" x={margin.left + step * (index + .5)} y={height - 7} textAnchor={index === 0 ? 'start' : index === candles.length - 1 ? 'end' : 'middle'}>{new Date(candles[index].open_time).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })}</text>)}
  </svg>
}
