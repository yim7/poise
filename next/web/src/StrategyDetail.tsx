// 运行详情（设计第 11 节）：批次表、格子表、在场挂单、等待原因、参数版本，
// 以及暂停、恢复、停止三个动作（第 2.4 节，B45）。

import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'

import { api, pauseSummary, resumeSummary, stopSummary } from './api'
import { decimal, percent, timestamp } from './format'
import { kindLabel, statusLabel, waitReasonLabel } from './labels'
import { BatchTable, CellTable, ConfigVersionTable, OrdersTable } from './tables'
import { RunningEditor } from './RunningEditor'

const REFRESH_MS = 3000

interface StrategyDetailViewProps {
  strategyId: string
  onBack: () => void
}

export function StrategyDetailView({ strategyId, onBack }: StrategyDetailViewProps) {
  const detail = useQuery({
    queryKey: ['strategy', strategyId],
    queryFn: () => api.strategy(strategyId),
    refetchInterval: REFRESH_MS,
  })
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [confirmingStop, setConfirmingStop] = useState(false)
  const client = useQueryClient()

  /** 三个动作都要等下一轮读数：做完立刻刷新，页面不会停在旧状态。 */
  async function run(action: () => Promise<string>) {
    setBusy(true)
    setError(null)
    setNotice(null)
    try {
      setNotice(await action())
      await client.invalidateQueries({ queryKey: ['strategy', strategyId] })
      await client.invalidateQueries({ queryKey: ['strategies'] })
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure))
    } finally {
      setBusy(false)
      setConfirmingStop(false)
    }
  }

  if (detail.data === undefined) {
    return (
      <section>
        <button type="button" onClick={onBack}>
          返回列表
        </button>
        {detail.error === null ? <p>读取中…</p> : <p className="error">读取失败：{detail.error.message}</p>}
      </section>
    )
  }

  const strategy = detail.data
  const summary = strategy.summary
  const status = summary.status
  const archived = status === 'stopped'
  const running = status === 'running'
  // 过渡状态：后台正在撤单、按 cloid 查终态、补记成交，一张新单都不下，走完自己转成
  // 「已暂停」「已归档」或「运行中」。用户要能看到它还在走，以及为什么还没走完。
  const settling = status === 'restoring' || status === 'pausing' || status === 'stopping'
  // 「会被暂停」只在服务端真的给出了暂停原因时才算数。仓位和应有持仓对不上但数据还在变
  // 的时候，服务端说的是「下一步再核对」，不会暂停：按对不上就报暂停会吓着用户。
  const willPause = running && strategy.waiting.includes('position_check_failed')

  return (
    <section className="detail">
      <div className="actions">
        <button type="button" onClick={onBack}>
          返回列表
        </button>
        {running ? (
          <button
            type="button"
            disabled={busy}
            onClick={() =>
              run(async () => pauseSummary(await api.pause(strategyId)))
            }
          >
            暂停
          </button>
        ) : null}
        {/* 已暂停、暂停中、恢复中都能恢复：暂停中恢复是「改主意」，恢复中恢复是
            「现在就再试一次」，服务端两条都认。 */}
        {status === 'paused' || status === 'pausing' || status === 'restoring' ? (
          <button
            type="button"
            disabled={busy}
            onClick={() => run(async () => resumeSummary(await api.resume(strategyId)))}
          >
            恢复
          </button>
        ) : null}
        {/* 停止中再点一次也是同一个结果，不给按钮，让页面上的状态说明去交代进度。 */}
        {archived || status === 'stopping' ? null : confirmingStop ? (
          <>
            <button
              type="button"
              className="danger"
              disabled={busy}
              onClick={() =>
                run(async () => stopSummary(await api.stop(strategyId)))
              }
            >
              确认停止
            </button>
            <button type="button" disabled={busy} onClick={() => setConfirmingStop(false)}>
              取消
            </button>
          </>
        ) : (
          <button type="button" className="danger" onClick={() => setConfirmingStop(true)}>
            停止
          </button>
        )}
      </div>
      {notice === null ? null : <p className="notice">{notice}</p>}
      {error === null ? null : <p className="error">{error}</p>}

      <h2>
        {summary.instrument} · {kindLabel(summary.config)} · {statusLabel(summary.status)}
      </h2>
      <dl className="facts">
        <div>
          <dt>策略</dt>
          <dd>{summary.strategy_id}</dd>
        </div>
        <div>
          <dt>参数版本</dt>
          <dd>{summary.config_version}</dd>
        </div>
        <div>
          <dt>建立时间</dt>
          <dd>{timestamp(summary.created_at)}</dd>
        </div>
        <div>
          <dt>标记价</dt>
          <dd>{decimal(strategy.market_price)}</dd>
        </div>
        <div>
          <dt>可用 USDC</dt>
          <dd>{decimal(strategy.available_collateral)}</dd>
        </div>
        <div>
          <dt>账户保证金率</dt>
          <dd>{percent(strategy.margin_ratio)}</dd>
        </div>
        <div>
          <dt>交易所仓位</dt>
          <dd>{decimal(strategy.position)}</dd>
        </div>
        {running || settling ? (
          <div>
            <dt>应有持仓</dt>
            <dd>{decimal(strategy.expected_position)}</dd>
          </div>
        ) : null}
        {/* 行情、仓位、可用资金都是后台取回来的读数，不是点开这一页时现取的：
            标上时刻，页面才不会把一份几分钟前的读数当成现在。 */}
        <div>
          <dt>读数时刻</dt>
          <dd>{timestamp(strategy.taken_at)}</dd>
        </div>
      </dl>
      {willPause ? <p className="error">仓位核对失败：策略会被暂停，先核对再恢复。</p> : null}

      <h3>这一步在等什么</h3>
      {running ? (
        strategy.waiting.length === 0 ? (
          <p>没有等待：该挂的单都挂上了。</p>
        ) : (
          <ul className="waiting">
            {strategy.waiting.map((reason) => (
              <li key={reason}>{waitReasonLabel(reason)}</li>
            ))}
          </ul>
        )
      ) : settling ? (
        <p>{strategy.note ?? `${statusLabel(status)}：后台正在撤单和对账，期间不会下新单。`}</p>
      ) : (
        <p>策略{statusLabel(status)}，没有在等的事。</p>
      )}

      <BatchTable batches={strategy.batches} />
      <CellTable cells={strategy.cells} />
      <OrdersTable orders={strategy.orders} />
      <ConfigVersionTable versions={strategy.config_versions} />

      {archived ? (
        <p className="hint">策略已归档，只能只读查看。</p>
      ) : running ? (
        <RunningEditor
          key={summary.config_version}
          strategyId={strategyId}
          config={summary.config}
        />
      ) : (
        // 服务端只让运行中的策略改参数：过渡状态还在收尾，已暂停的改了也不会生效。
        <p className="hint">策略{statusLabel(status)}，现在改不了参数；恢复之后可以改限速间隔和观点失效设置。</p>
      )}
    </section>
  )
}
