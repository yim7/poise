import '@testing-library/jest-dom/vitest'
import { afterEach, vi } from 'vitest'
import { cleanup } from '@testing-library/react'

// 每个测试之后清掉 DOM 和 fetch 替身：测试之间不互相影响。
// vitest 没有开 globals，所以要显式注册。
afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})
