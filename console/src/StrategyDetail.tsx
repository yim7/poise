// 运行详情（设计第 11 节）：一屏看完一个策略。
//
// 从上到下按要紧程度排：标的和状态、关键读数和价格条、它现在在等什么；然后是格子阶梯、
// 在场挂单和主仓批次；最后是运行中可改的参数和参数版本。暂停、恢复、停止在右上角
// （第 2.4 节，B45）。

import { useRef, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'

import { KindBadge, Pill, StatusBadge } from '@/components/badges'
import { Callout } from '@/components/layout'
import { CloseDialog } from '@/components/close-dialog'
import { RangeBar } from '@/components/range-bar'
import { Stat, type StatTone, positionTone } from '@/components/stat'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { RunningEditor } from './RunningEditor'
import { api, closeSummary, pauseSummary, resumeSummary, stopSummary } from './api'
import { decimal, percent, timestamp } from './format'
import { isArchived, statusLabel, waitReasonLabel } from './labels'
import { BatchTable, CellLadder, ConfigVersionTable, OrdersTable, Panel } from './tables'
import type { StrategyDetail } from './types'

const REFRESH_MS = 3000

/** Unified 账户每天最多的用户操作次数（第 10 节），和服务端的上限一致。 */
const DAILY_ACTION_LIMIT = 50000

interface StrategyDetailViewProps {
  strategyId: string
  onBack: () => void
}

/** 金额按正负着色：收到、赚到是绿，付出是红。 */
function signTone(value: string | null): StatTone {
  const parsed = Number(value)
  if (value === null || !Number.isFinite(parsed) || parsed === 0) return 'default'
  return parsed > 0 ? 'long' : 'short'
}

export function StrategyDetailView({ strategyId, onBack }: StrategyDetailViewProps) {
  const detail = useQuery({
    queryKey: ['strategy', strategyId],
    queryFn: () => api.strategy(strategyId),
    refetchInterval: REFRESH_MS,
  })
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 正在进行的操作数：停止和紧接着的平仓会同时在途，一个结束不能提前解锁按钮。
  const [inFlight, setInFlight] = useState(0)
  const busy = inFlight > 0
  // 还没返回的停止请求：平仓要等它成功之后再发（服务端最多等 3 秒，撤单常常更久）。
  const pendingStop = useRef<Promise<unknown> | null>(null)
  // 停止确认之后的第二次确认：要不要市价平仓（第 7.7 节）。
  const [askClose, setAskClose] = useState(false)
  const client = useQueryClient()

  /**
   * 动作做完立刻刷新，页面不会停在旧状态。失败了也刷新：响应丢了的时候，服务端可能已经
   * 执行过了（例如停止其实已经开始）。
   */
  async function run(action: () => Promise<string>) {
    setInFlight((count) => count + 1)
    setError(null)
    setNotice(null)
    try {
      setNotice(await action())
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure))
    } finally {
      setInFlight((count) => count - 1)
      await client.invalidateQueries({ queryKey: ['strategy', strategyId] })
      await client.invalidateQueries({ queryKey: ['strategies'] })
    }
  }

  const back = (
    <Button variant="ghost" size="sm" className="-ml-2 w-fit text-muted-foreground" onClick={onBack}>
      ← 返回列表
    </Button>
  )

  if (detail.data === undefined) {
    return (
      <div className="flex flex-col gap-4">
        {back}
        {detail.error === null ? (
          <p className="text-xs text-muted-foreground">读取中…</p>
        ) : (
          <Callout tone="danger" title="读取失败">
            {detail.error.message}
          </Callout>
        )}
      </div>
    )
  }

  const strategy = detail.data
  const summary = strategy.summary
  const status = summary.status
  // 已失效、已平仓和已归档一样只读：价格回来也不自动恢复（第 7.5 节）。
  const archived = isArchived(status)
  const running = status === 'running'
  // 过渡状态：后台正在撤单、按 cloid 查终态、补记成交，走完自己转成「已暂停」「已归档」
  // 「运行中」「已失效」或「已平仓」。失效中、平仓中除了撤单还在用只减仓单减仓。
  const settling =
    status === 'restoring' ||
    status === 'pausing' ||
    status === 'stopping' ||
    status === 'invalidating' ||
    status === 'closing'
  const canResume = status === 'paused' || status === 'pausing' || status === 'restoring'
  const canStop = !archived && status !== 'stopping'
  const hasPosition = Number(strategy.position) !== 0
  // 停止中按这一步的快照；已归档的快照停在归档那一刻，按常驻服务每分钟的新读数
  // （第 7.7 节第 8 条）。还在运行的走「停止」之后的第二次确认。
  const held = summary.held
  const canClose = archived ? held !== null : status === 'stopping' && hasPosition
  const closing = archived && held !== null
    ? { position: held.position, mark: held.market_price }
    : { position: strategy.position, mark: strategy.market_price }

  /** 平仓：停止中补上平仓标记，已归档进入平仓中。紧跟在停止后面时，先等停止成功。 */
  const close = () =>
    run(async () => {
      const stop = pendingStop.current
      if (stop !== null) {
        try {
          await stop
        } catch {
          throw new Error('停止没成功，没有平仓。')
        } finally {
          pendingStop.current = null
        }
      }
      return closeSummary(await api.close(strategyId))
    })

  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-col gap-2">
        {back}
        <header aria-label="策略概况" className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex min-w-0 flex-wrap items-center gap-2.5">
            <h1 className="font-heading text-3xl font-semibold tracking-tight">{summary.instrument}</h1>
            <KindBadge config={summary.config} />
            <StatusBadge status={status} />
            <span className="num text-[11px] text-muted-foreground" title="策略标识">
              {summary.strategy_id}
            </span>
          </div>
          <div className="flex items-center gap-2">
            {running ? (
              <Button
                variant="outline"
                size="lg"
                disabled={busy}
                onClick={() => run(async () => pauseSummary(await api.pause(strategyId)))}
              >
                暂停
              </Button>
            ) : null}
            {/* 已暂停、暂停中、恢复中都能恢复：暂停中恢复是「改主意」，恢复中恢复是
                「现在就再试一次」，服务端两条都认。 */}
            {canResume ? (
              <Button
                size="lg"
                disabled={busy}
                onClick={() => run(async () => resumeSummary(await api.resume(strategyId)))}
              >
                恢复
              </Button>
            ) : null}
            {canClose ? (
              <Button variant="destructive" size="lg" disabled={busy} onClick={() => setAskClose(true)}>
                市价平仓
              </Button>
            ) : null}
            {/* 停止中再点一次也是同一个结果，不给按钮，让状态说明去交代进度。 */}
            {canStop ? (
              <StopButton
                busy={busy}
                status={status}
                onConfirm={() => {
                  // 平仓中停止就是放弃平仓；其余有仓位的，紧接着问要不要平（第 7.7 节）。弹窗
                  // 不等停止请求回来（撤单要好几秒），确认之后的平仓请求等它成功再发。
                  const stop = api.stop(strategyId)
                  pendingStop.current = stop
                  if (hasPosition && status !== 'closing') setAskClose(true)
                  void run(async () => stopSummary(await stop))
                }}
              />
            ) : null}
            <CloseDialog
              open={askClose}
              onOpenChange={(open) => {
                setAskClose(open)
                // 第二次确认关掉了（保留仓位，或者已经确认过平仓）：和那次停止的关联到此为止。
                // 之后再点市价平仓是独立的操作，不该再等那次停止的结果。确认平仓时，平仓已经
                // 在关窗之前取走了这次停止。
                if (!open) pendingStop.current = null
              }}
              position={closing.position}
              mark={closing.mark}
              onConfirm={close}
            />
          </div>
        </header>
      </div>

      {notice === null ? null : <Callout tone="ok">{notice}</Callout>}
      {error === null ? null : (
        <Callout tone="danger" title="操作失败">
          {error}
        </Callout>
      )}

      <Readings strategy={strategy} showExpected={running || settling} />

      <WhatsHappening strategy={strategy} running={running} settling={settling} />

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Panel title="网格格子" count={strategy.cells.length}>
          <CellLadder cells={strategy.cells} mark={strategy.market_price} />
        </Panel>
        <div className="flex flex-col gap-4">
          <Panel title="在场挂单" count={strategy.orders.length}>
            <OrdersTable orders={strategy.orders} />
          </Panel>
          {strategy.batches.length === 0 ? null : (
            <Panel title="主仓批次" count={strategy.batches.length}>
              <BatchTable batches={strategy.batches} />
            </Panel>
          )}
        </div>
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <div>
          <EditorSection strategy={strategy} strategyId={strategyId} />
        </div>
        <Panel title="参数版本" count={strategy.config_versions.length}>
          <ConfigVersionTable versions={strategy.config_versions} />
        </Panel>
      </div>
    </div>
  )
}

function StopButton({
  busy,
  status,
  onConfirm,
}: {
  busy: boolean
  status: StrategyDetail['summary']['status']
  onConfirm: () => void
}) {
  return (
    <AlertDialog>
      <AlertDialogTrigger asChild>
        <Button variant="destructive" size="lg" disabled={busy}>
          停止
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>停止策略？</AlertDialogTitle>
          <AlertDialogDescription>
            {status === 'invalidating'
              ? '撤销全部挂单，放弃剩余减仓。'
              : status === 'closing'
                ? '撤销平仓单，放弃剩余平仓，仓位保留。'
                : '撤销全部挂单。'}
            停止后归档，不可恢复。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>取消</AlertDialogCancel>
          <AlertDialogAction variant="destructive" onClick={onConfirm}>
            确认停止
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}

/** 关键读数：大数字一行，次要的读数一行，下面是价格条。全部来自后台最近一步的快照。 */
function Readings({ strategy, showExpected }: { strategy: StrategyDetail; showExpected: boolean }) {
  const summary = strategy.summary
  const mismatch = showExpected && strategy.position !== strategy.expected_position
  return (
    <section aria-label="关键读数" className="flex flex-col gap-4 rounded-lg bg-card p-4 ring-1 ring-border">
      <div className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-3 lg:grid-cols-6">
        <Stat label="标记价" value={decimal(strategy.market_price)} tone="signal" size="lg" />
        <Stat
          label="交易所仓位"
          value={decimal(strategy.position)}
          tone={positionTone(strategy.position)}
          size="lg"
          // 和应有持仓一致时不写：只有对不上才值得看。
          hint={
            mismatch ? (
              <span className="text-warn">
                应有持仓 <span className="num">{decimal(strategy.expected_position)}</span>
              </span>
            ) : undefined
          }
        />
        <Stat
          label="持仓均价"
          value={decimal(strategy.average_price)}
          tone={strategy.average_price === null ? 'muted' : 'default'}
          size="lg"
        />
        {/* 保本价把网格利润、减仓盈亏和资金费都摊进持仓：标记价回到这里，合计正好为零。 */}
        <Stat
          label="保本价"
          value={decimal(strategy.breakeven_price)}
          tone={strategy.breakeven_price === null ? 'muted' : 'default'}
          size="lg"
        />
        <Stat
          label="浮动盈亏（估）"
          value={decimal(strategy.unrealized_pnl)}
          tone={signTone(strategy.unrealized_pnl)}
          size="lg"
        />
        <Stat
          label="合计（估）"
          value={decimal(strategy.total_pnl)}
          tone={signTone(strategy.total_pnl)}
          size="lg"
        />
        <Stat
          label="网格利润（估）"
          value={decimal(strategy.grid_realized_profit)}
          tone={signTone(strategy.grid_realized_profit)}
          size="lg"
        />
        {/* 只有失效减仓或市价平仓成交过才有：没有时不占位置。 */}
        {strategy.reduction_realized_profit === null ? null : (
          <Stat
            label="减仓已实现（估）"
            value={decimal(strategy.reduction_realized_profit)}
            tone={signTone(strategy.reduction_realized_profit)}
            size="lg"
          />
        )}
        <Stat
          label="累计资金费"
          value={strategy.funding_synced_through === null ? '未同步' : decimal(strategy.funding_total)}
          tone={strategy.funding_synced_through === null ? 'muted' : signTone(strategy.funding_total)}
          size="lg"
          hint={
            strategy.funding_synced_through === null
              ? undefined
              : `截至 ${timestamp(strategy.funding_synced_through)}`
          }
        />
        <Stat
          label="账户保证金率"
          value={percent(strategy.margin_ratio)}
          tone={strategy.margin_ratio === null ? 'muted' : 'default'}
          size="lg"
        />
        <Stat label="可用 USDC" value={decimal(strategy.available_collateral)} size="lg" />
      </div>

      <RangeBar config={summary.config} mark={strategy.market_price} />

      <dl className="grid grid-cols-1 gap-x-6 gap-y-1.5 border-t border-border pt-3 text-xs sm:grid-cols-2 lg:grid-cols-3">
        {/* 资金费每小时结算一次：费率按每小时给，年化只是换算，方便和阈值对照。 */}
        <Fact label="资金费率">
          {strategy.funding_rate === null || strategy.funding_rate_read_at === null ? (
            '读不到'
          ) : (
            <span title={`读于 ${timestamp(strategy.funding_rate_read_at)}`}>
              {`${percent(strategy.funding_rate.hourly)}/时 · 年化 ${percent(strategy.funding_rate.annual)}`}
            </span>
          )}
        </Fact>
        {/* 利润是按计划价的估算，比例只用来看资金费吃掉了多少利润（第 7.6 节）。 */}
        <Fact label="资金费 / 网格利润">
          {strategy.funding_share === null ? '—' : percent(strategy.funding_share)}
        </Fact>
        {/* 全账户的计数：同一账户的所有策略共用每天 5 万次的上限，超过 80% 时说明里会提醒。 */}
        <Fact label="今日操作次数">
          {strategy.actions_today} / {DAILY_ACTION_LIMIT}
        </Fact>
        {/* 行情、仓位、可用资金都是后台取回来的读数，不是点开这一页时现取的：标上时刻。 */}
        <Fact label="读数时刻">{timestamp(strategy.taken_at)}</Fact>
        <Fact label="参数版本">v{summary.config_version}</Fact>
        <Fact label="建立时间">{timestamp(summary.created_at)}</Fact>
      </dl>
    </section>
  )
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="shrink-0 text-muted-foreground">{label}</dt>
      <dd className="num min-w-0 text-right break-words">{children}</dd>
    </div>
  )
}

/** 这个策略此刻在干什么、在等什么：最要紧的一句话放在读数下面。 */
function WhatsHappening({
  strategy,
  running,
  settling,
}: {
  strategy: StrategyDetail
  running: boolean
  settling: boolean
}) {
  const status = strategy.summary.status
  // 「会被暂停」只在服务端真的给出了暂停原因时才算数：数据还在变时服务端说的是「下一步
  // 再核对」，按对不上就报暂停会吓着用户。
  const willPause = running && strategy.waiting.includes('position_check_failed')
  // 没在等、也没有说明时这一块不出现：正常运行不用说话，状态标识已经说明了一切。
  const quiet = strategy.note === null && (running ? strategy.waiting.length === 0 : !settling)
  if (quiet) return null

  return (
    <section aria-label="等待" className="flex flex-col gap-2">
      <h2 className="eyebrow">等待</h2>
      {willPause ? (
        <Callout tone="danger" title="仓位核对失败，即将暂停" />
      ) : null}
      {running ? (
        <>
          {strategy.waiting.length === 0 ? null : (
            <ul className="flex flex-wrap gap-1.5">
              {strategy.waiting.map((reason) => (
                <li key={reason}>
                  <Pill tone={reason === 'position_check_failed' ? 'danger' : 'warn'}>{waitReasonLabel(reason)}</Pill>
                </li>
              ))}
            </ul>
          )}
          {/* 运行中的说明：结果未知订单缺什么、保证金率、资金费率、操作次数的提醒。 */}
          {strategy.note === null ? null : <Callout tone="warn">{strategy.note}</Callout>}
        </>
      ) : settling ? (
        <Callout tone={status === 'invalidating' ? 'danger' : 'warn'}>
          {strategy.note ?? `${statusLabel(status)}：撤单、对账中，不下新单。`}
        </Callout>
      ) : (
        // 已暂停的说明里有暂停原因和资金费提醒，已失效的说明里有没减掉的尾量，都要让用户看到。
        <Callout tone={status === 'invalidated' ? 'danger' : 'info'}>{strategy.note}</Callout>
      )}
    </section>
  )
}

/** 运行中可改的参数；不在运行时说明为什么改不了。 */
function EditorSection({ strategy, strategyId }: { strategy: StrategyDetail; strategyId: string }) {
  const summary = strategy.summary
  const status = summary.status
  const hasInvalidation = summary.config.long?.invalidation != null || summary.config.short?.invalidation != null
  if (status === 'invalidated') {
    return (
      <Callout tone="info">已失效，只读。新建策略可接管剩余仓位。</Callout>
    )
  }
  if (status === 'stopped') return <Callout tone="info">已归档，只读。</Callout>
  if (status === 'closed') return <Callout tone="info">已平仓，只读。</Callout>
  if (status === 'invalidating') {
    return <Callout tone="warn">失效减仓中，参数不可改。停止即放弃减仓。</Callout>
  }
  if (status === 'closing') {
    return <Callout tone="warn">平仓中，参数不可改。停止即放弃平仓。</Callout>
  }
  if (status === 'running') {
    return <RunningEditor key={summary.config_version} strategyId={strategyId} config={summary.config} />
  }
  // 服务端只让运行中的策略改参数：过渡状态还在收尾，已暂停的改了也不会生效。
  return (
    <Callout tone="info">
      {statusLabel(status)}，恢复后可改限速和失效设置。
      {hasInvalidation && status !== 'stopping' ? '暂停期间越过失效价照样减仓。' : null}
    </Callout>
  )
}
