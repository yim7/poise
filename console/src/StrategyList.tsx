// 策略列表（设计第 11 节）：活动策略按卡片排，草稿单独一栏，已归档的默认收起。
//
// 列表只读服务端算好的汇总，不再去交易所取数：行情是后台最近一步读到的那一份。

import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'

import { KindBadge, Pill, StatusBadge } from '@/components/badges'
import { CloseDialog } from '@/components/close-dialog'
import { Callout, PageHeader, SectionTitle } from '@/components/layout'
import { Private, usePrivacy } from '@/components/privacy'
import { RangeBar } from '@/components/range-bar'
import { Stat, positionTone } from '@/components/stat'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { api, closeSummary } from './api'
import { configSummary, decimal, timestamp } from './format'
import { isArchived as isArchivedStatus } from './labels'
import type { StoredDraft, StrategySummary } from './types'

const REFRESH_MS = 5000

interface StrategyListProps {
  onOpenStrategy: (id: string) => void
  onOpenDraft: (id: string) => void
  onNew: () => void
  onOpenSettings: () => void
}

function isArchived(strategy: StrategySummary): boolean {
  return isArchivedStatus(strategy.status)
}

export function StrategyList({
  onOpenStrategy,
  onOpenDraft,
  onNew,
  onOpenSettings,
}: StrategyListProps) {
  const strategies = useQuery({
    queryKey: ['strategies'],
    queryFn: api.strategies,
    refetchInterval: REFRESH_MS,
  })
  const drafts = useQuery({
    queryKey: ['drafts'],
    queryFn: api.drafts,
    refetchInterval: REFRESH_MS,
  })
  const [showArchived, setShowArchived] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  const [closeError, setCloseError] = useState<string | null>(null)
  const client = useQueryClient()

  const failure = strategies.error ?? drafts.error
  const live = strategies.data?.filter((strategy) => !isArchived(strategy)) ?? []
  // 已归档、标的上仍有仓位的单独一栏，不收起（第 11 节）；其余已归档的收起。
  const held = strategies.data?.filter((strategy) => isArchived(strategy) && strategy.held !== null) ?? []
  const archived = strategies.data?.filter((strategy) => isArchived(strategy) && strategy.held === null) ?? []

  async function close(strategyId: string) {
    setNotice(null)
    setCloseError(null)
    try {
      setNotice(closeSummary(await api.close(strategyId)))
      await client.invalidateQueries({ queryKey: ['strategies'] })
    } catch (error) {
      setCloseError(error instanceof Error ? error.message : String(error))
    }
  }
  const running = live.filter((strategy) => strategy.status === 'running').length

  return (
    <div className="flex flex-col gap-8">
      <PageHeader
        title="策略"
        subtitle={
          strategies.data === undefined
            ? '读取中…'
            : `${live.length} 个活动 · ${running} 个运行中`
        }
        actions={
          <>
            <Button variant="ghost" size="lg" onClick={onOpenSettings}>
              账户风控设置
            </Button>
            <Button size="lg" onClick={onNew}>
              新建策略
            </Button>
          </>
        }
      />
      {failure === null || failure === undefined ? null : (
        <Callout tone="danger" title="读取失败">
          {failure.message}
        </Callout>
      )}

      {/* 平仓的结果放在这里而不是「仍有仓位」里：开始平仓之后那一行就移到活动策略了。 */}
      {notice === null ? null : <Callout tone="ok">{notice}</Callout>}
      {closeError === null ? null : (
        <Callout tone="danger" title="平仓失败">
          {closeError}
        </Callout>
      )}

      <section aria-label="活动策略">
        {strategies.data === undefined ? null : live.length === 0 ? (
          <EmptyLive onNew={onNew} />
        ) : (
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
            {live.map((strategy) => (
              <StrategyCard
                key={strategy.strategy_id}
                strategy={strategy}
                onOpen={() => onOpenStrategy(strategy.strategy_id)}
              />
            ))}
          </div>
        )}
      </section>

      {held.length === 0 ? null : (
        <section aria-label="仍有仓位">
          <SectionTitle count={held.length}>仍有仓位</SectionTitle>
          <div className="overflow-hidden rounded-lg ring-1 ring-warn/40">
            {held.map((strategy) => (
              <HeldRow
                key={strategy.strategy_id}
                strategy={strategy}
                onOpen={() => onOpenStrategy(strategy.strategy_id)}
                onClose={() => close(strategy.strategy_id)}
              />
            ))}
          </div>
        </section>
      )}

      <section aria-label="草稿">
        <SectionTitle count={drafts.data?.length}>草稿</SectionTitle>
        {drafts.data === undefined ? (
          <p className="text-xs text-muted-foreground">读取中…</p>
        ) : drafts.data.length === 0 ? (
          <p className="text-xs text-muted-foreground">无草稿</p>
        ) : (
          <div className="overflow-hidden rounded-lg ring-1 ring-border">
            {drafts.data.map((draft) => (
              <DraftRow key={draft.draft_id} draft={draft} onOpen={() => onOpenDraft(draft.draft_id)} />
            ))}
          </div>
        )}
      </section>

      {archived.length === 0 ? null : (
        <section aria-label="已归档">
          <SectionTitle
            count={archived.length}
            action={
              <Button variant="ghost" size="sm" onClick={() => setShowArchived((shown) => !shown)}>
                {showArchived ? '收起' : '展开'}
              </Button>
            }
          >
            已归档
          </SectionTitle>
          {showArchived ? (
            <div className="overflow-hidden rounded-lg ring-1 ring-border">
              {archived.map((strategy) => (
                <ArchivedRow
                  key={strategy.strategy_id}
                  strategy={strategy}
                  onOpen={() => onOpenStrategy(strategy.strategy_id)}
                />
              ))}
            </div>
          ) : null}
        </section>
      )}
    </div>
  )
}

function EmptyLive({ onNew }: { onNew: () => void }) {
  return (
    <div className="flex flex-col items-start gap-3 rounded-lg border border-dashed border-border px-6 py-10">
      <div className="font-heading text-base font-semibold">没有活动策略</div>
      <Button size="lg" onClick={onNew}>
        新建策略
      </Button>
    </div>
  )
}

function StrategyCard({ strategy, onOpen }: { strategy: StrategySummary; onOpen: () => void }) {
  return (
    <article
      className="group flex cursor-pointer flex-col gap-4 rounded-lg bg-card p-4 ring-1 ring-border transition-colors hover:ring-signal/40"
      onClick={onOpen}
    >
      <header className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="truncate font-heading text-xl font-semibold tracking-tight">
            {strategy.instrument}
          </span>
          <KindBadge config={strategy.config} />
        </div>
        <StatusBadge status={strategy.status} />
      </header>
      <RangeBar config={strategy.config} mark={strategy.market_price} cells={strategy.cells} />
      <div className="grid grid-cols-3 gap-3">
        <Stat label="净仓位" value={<Private>{decimal(strategy.position)}</Private>} tone={positionTone(strategy.position)} />
        <Stat label="在场挂单" value={strategy.working_orders} />
        <Stat label="参数版本" value={`v${strategy.config_version}`} tone="muted" />
      </div>
      <footer className="flex items-center justify-between border-t border-border pt-3">
        <span className="text-[11px] text-muted-foreground">
          更新 <span className="num">{timestamp(strategy.updated_at)}</span>
        </span>
        <Button
          variant="outline"
          size="sm"
          onClick={(event) => {
            event.stopPropagation()
            onOpen()
          }}
        >
          查看
        </Button>
      </footer>
    </article>
  )
}

function DraftRow({ draft, onOpen }: { draft: StoredDraft; onOpen: () => void }) {
  const { hidden } = usePrivacy()
  return (
    <div className="flex items-center gap-3 border-b border-border bg-card px-4 py-3 last:border-b-0">
      <span className="w-20 shrink-0 font-heading text-sm font-semibold">{draft.config.instrument}</span>
      <KindBadge config={draft.config} />
      <Pill tone="info">草稿</Pill>
      <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground" title={configSummary(draft.config, hidden)}>
        {configSummary(draft.config, hidden)}
      </span>
      <span className="hidden shrink-0 text-[11px] text-muted-foreground num sm:inline">
        {timestamp(draft.updated_at)}
      </span>
      <Button variant="outline" size="sm" onClick={onOpen}>
        编辑
      </Button>
    </div>
  )
}

/** 已归档、标的上仍有仓位的一行：净仓位按方向着色，带读数时刻和市价平仓。 */
function HeldRow({
  strategy,
  onOpen,
  onClose,
}: {
  strategy: StrategySummary
  onOpen: () => void
  onClose: () => void
}) {
  const [asking, setAsking] = useState(false)
  const held = strategy.held
  if (held === null) return null
  return (
    <div className="flex flex-wrap items-center gap-3 border-b border-border bg-card px-4 py-2.5 last:border-b-0">
      <span className="w-20 shrink-0 font-heading text-sm font-semibold">{strategy.instrument}</span>
      <StatusBadge status={strategy.status} />
      <span className={cn('num text-sm font-medium', positionTone(held.position) === 'short' ? 'text-short' : 'text-long')}>
        <Private>{decimal(held.position)}</Private>
      </span>
      <span className="min-w-0 flex-1 text-[11px] text-muted-foreground num" title="仓位读数时刻">
        {timestamp(held.read_at)}
      </span>
      <Button variant="ghost" size="sm" onClick={onOpen}>
        查看
      </Button>
      <Button variant="destructive" size="sm" onClick={() => setAsking(true)}>
        市价平仓
      </Button>
      <CloseDialog
        open={asking}
        onOpenChange={setAsking}
        position={held.position}
        mark={held.market_price}
        onConfirm={onClose}
      />
    </div>
  )
}

function ArchivedRow({ strategy, onOpen }: { strategy: StrategySummary; onOpen: () => void }) {
  const { hidden } = usePrivacy()
  return (
    <div className="flex items-center gap-3 border-b border-border bg-card/60 px-4 py-2.5 last:border-b-0">
      <span className="w-20 shrink-0 font-heading text-sm font-semibold text-muted-foreground">
        {strategy.instrument}
      </span>
      <KindBadge config={strategy.config} />
      <StatusBadge status={strategy.status} />
      <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground" title={configSummary(strategy.config, hidden)}>
        {configSummary(strategy.config, hidden)}
      </span>
      <span className="hidden shrink-0 text-[11px] text-muted-foreground num sm:inline">
        {timestamp(strategy.updated_at)}
      </span>
      <Button variant="ghost" size="sm" onClick={onOpen}>
        查看
      </Button>
    </div>
  )
}
