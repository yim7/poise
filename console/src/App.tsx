// 控制台外壳：顶栏（品牌、导航、网络标识）加当前这一屏。
//
// 当前在哪一屏记在地址的 hash 里（`route.ts`）：刷新、前进后退都回到原处。

import { useQuery } from '@tanstack/react-query'

import { NetworkBadge } from '@/components/badges'
import { AccountAddress, PrivacyToggle } from '@/components/privacy'
import { cn } from '@/lib/utils'
import { AccountSettingsView } from './AccountSettings'
import { DraftEditor } from './DraftEditor'
import { StrategyDetailView } from './StrategyDetail'
import { StrategyList } from './StrategyList'
import { api } from './api'
import { type Route, replaceHash, useRoute } from './route'

export function App() {
  const [route, navigate] = useRoute()
  const instance = useQuery({ queryKey: ['instance'], queryFn: api.instance, staleTime: Infinity })
  const mainnet = instance.data?.network === 'mainnet'

  return (
    <div className="min-h-screen">
      {/* 正式网：整页顶上一条琥珀色细线，进到哪一屏都看得见这是实盘。 */}
      {mainnet ? <div className="fixed inset-x-0 top-0 z-50 h-0.5 bg-warn" /> : null}
      <header className="sticky top-0 z-40 border-b border-border bg-background/85 backdrop-blur">
        <div className="mx-auto flex h-12 max-w-[1240px] items-center gap-6 px-5">
          <button
            type="button"
            className="flex items-center gap-2"
            onClick={() => navigate({ name: 'list' })}
            aria-label="回到策略列表"
          >
            <Logo />
            <span className="font-heading text-[15px] font-semibold tracking-tight">Poise</span>
          </button>
          <nav className="flex items-center gap-1 text-xs">
            <NavLink active={route.name !== 'settings'} onClick={() => navigate({ name: 'list' })}>
              策略
            </NavLink>
            <NavLink active={route.name === 'settings'} onClick={() => navigate({ name: 'settings' })}>
              风控设置
            </NavLink>
          </nav>
          <div className="ml-auto flex items-center gap-3">
            <PrivacyToggle />
            {instance.data?.account_address ? (
              <AccountAddress address={instance.data.account_address} />
            ) : null}
            <NetworkBadge instance={instance.data} />
          </div>
        </div>
      </header>
      <main className="mx-auto max-w-[1240px] px-5 pt-6 pb-16">
        <Screen route={route} navigate={navigate} />
      </main>
    </div>
  )
}

function Screen({ route, navigate }: { route: Route; navigate: (route: Route) => void }) {
  const toList = () => navigate({ name: 'list' })
  const toStrategy = (id: string) => navigate({ name: 'strategy', id })
  switch (route.name) {
    case 'list':
      return (
        <StrategyList
          onNew={() => navigate({ name: 'new' })}
          onOpenDraft={(id) => navigate({ name: 'draft', id })}
          onOpenStrategy={toStrategy}
          onOpenSettings={() => navigate({ name: 'settings' })}
        />
      )
    case 'new':
      return (
        <DraftEditor
          key="new"
          draftId={null}
          onBack={toList}
          onStarted={toStrategy}
          onCreated={(id) => replaceHash({ name: 'draft', id })}
        />
      )
    case 'draft':
      return <DraftEditor key={route.id} draftId={route.id} onBack={toList} onStarted={toStrategy} />
    case 'settings':
      return <AccountSettingsView onBack={toList} />
    case 'strategy':
      return <StrategyDetailView key={route.id} strategyId={route.id} onBack={toList} />
  }
}

function NavLink({
  active,
  onClick,
  children,
}: {
  active: boolean
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        'rounded-md px-2.5 py-1.5 transition-colors',
        active ? 'bg-muted text-foreground' : 'text-muted-foreground hover:text-foreground',
      )}
    >
      {children}
    </button>
  )
}

/** 标志：两条错开的横线，一高一低，像一格网格的买和卖。 */
function Logo() {
  return (
    <svg viewBox="0 0 20 20" className="size-5" aria-hidden="true">
      <rect x="2" y="5" width="11" height="3" rx="1.5" className="fill-signal" />
      <rect x="7" y="12" width="11" height="3" rx="1.5" className="fill-foreground/70" />
    </svg>
  )
}
