// 隐藏金额：截图、共享屏幕时遮住能看出账户资金量的数——仓位、数量、盈亏、可用资金。
//
// 价格、比率、计数不遮：单看它们推不出账户有多大。开关只管显示，记在本机浏览器里，
// 刷新后保持；读写不了本地存储时按「不隐藏」处理。输入框不遮：填参数时要看得见自己填的数。

import { createContext, useContext, useState } from 'react'

const STORAGE_KEY = 'poise.hide-amounts'

/** 遮住之后显示的字样。 */
export const MASK = '••••'

interface Privacy {
  hidden: boolean
  setHidden: (hidden: boolean) => void
}

const PrivacyContext = createContext<Privacy>({ hidden: false, setHidden: () => {} })

function stored(): boolean {
  try {
    return window.localStorage.getItem(STORAGE_KEY) === '1'
  } catch {
    return false
  }
}

export function PrivacyProvider({ children }: { children: React.ReactNode }) {
  const [hidden, setState] = useState(stored)
  const setHidden = (next: boolean) => {
    setState(next)
    try {
      window.localStorage.setItem(STORAGE_KEY, next ? '1' : '0')
    } catch {
      // 记不住只影响刷新之后，这一次照样生效。
    }
  }
  return <PrivacyContext.Provider value={{ hidden, setHidden }}>{children}</PrivacyContext.Provider>
}

export function usePrivacy(): Privacy {
  return useContext(PrivacyContext)
}

/** 金额类的显示：隐藏时换成遮罩。 */
export function Private({ children }: { children: React.ReactNode }) {
  const { hidden } = usePrivacy()
  return <>{hidden ? MASK : children}</>
}

/**
 * 服务端写好的说明文字（暂停原因、平仓进度、预览的校验和提醒）里夹着仓位和数量，分不出
 * 哪个数是价格、哪个是数量：隐藏时除百分比以外的数都遮住，要看全文就关掉开关。
 */
export function maskNumbers(text: string): string {
  return text.replace(/\d+(?:\.\d+)?(?![\d.]*%)/g, MASK)
}

/** 服务端说明文字的显示：隐藏时遮住其中的数。 */
export function PrivateText({ children }: { children: string }) {
  const { hidden } = usePrivacy()
  return <>{hidden ? maskNumbers(children) : children}</>
}

/** 顶栏上的开关。 */
export function PrivacyToggle() {
  const { hidden, setHidden } = usePrivacy()
  return (
    <button
      type="button"
      aria-pressed={hidden}
      title={hidden ? '显示仓位、数量和盈亏' : '隐藏仓位、数量和盈亏'}
      onClick={() => setHidden(!hidden)}
      className="flex items-center gap-1.5 rounded-md px-2 py-1 text-[11px] text-muted-foreground transition-colors hover:text-foreground"
    >
      <EyeIcon closed={hidden} />
      {hidden ? '显示金额' : '隐藏金额'}
    </button>
  )
}

function EyeIcon({ closed }: { closed: boolean }) {
  return (
    <svg viewBox="0 0 16 16" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
      <path d="M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8Z" />
      <circle cx="8" cy="8" r="2" />
      {closed ? <path d="M2.5 13.5 13.5 2.5" /> : null}
    </svg>
  )
}
