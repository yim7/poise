// 控制台外壳：列表、新建、草稿、运行详情、账户风控设置几个屏之间切换。
//
// 没有路由库：屏少，状态就够了；刷新页面回到列表。

import { useState } from 'react'

import { AccountSettingsView } from './AccountSettings'
import { DraftEditor } from './DraftEditor'
import { StrategyDetailView } from './StrategyDetail'
import { StrategyList } from './StrategyList'

type Screen =
  | { name: 'list' }
  | { name: 'new' }
  | { name: 'draft'; id: string }
  | { name: 'strategy'; id: string }
  | { name: 'settings' }

export function App() {
  const [screen, setScreen] = useState<Screen>({ name: 'list' })

  return (
    <main>
      <header>
        <h1>Poise 网格策略</h1>
        <p className="hint">只在本机运行；私钥只在启动文件里，网页不读也不显示。</p>
      </header>
      {screen.name === 'list' ? (
        <StrategyList
          onNew={() => setScreen({ name: 'new' })}
          onOpenDraft={(id) => setScreen({ name: 'draft', id })}
          onOpenStrategy={(id) => setScreen({ name: 'strategy', id })}
          onOpenSettings={() => setScreen({ name: 'settings' })}
        />
      ) : null}
      {screen.name === 'new' ? (
        <DraftEditor
          draftId={null}
          onBack={() => setScreen({ name: 'list' })}
          onStarted={(id) => setScreen({ name: 'strategy', id })}
        />
      ) : null}
      {screen.name === 'draft' ? (
        <DraftEditor
          draftId={screen.id}
          onBack={() => setScreen({ name: 'list' })}
          onStarted={(id) => setScreen({ name: 'strategy', id })}
        />
      ) : null}
      {screen.name === 'settings' ? (
        <AccountSettingsView onBack={() => setScreen({ name: 'list' })} />
      ) : null}
      {screen.name === 'strategy' ? (
        <StrategyDetailView strategyId={screen.id} onBack={() => setScreen({ name: 'list' })} />
      ) : null}
    </main>
  )
}
