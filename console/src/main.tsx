import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'

import { PrivacyProvider } from '@/components/privacy'
import { App } from './App'
import './styles.css'

// 常驻服务每几秒走一步，页面按自己的节奏取数就够了；窗口重新获得焦点时立刻取一次。
const client = new QueryClient({
  defaultOptions: { queries: { refetchOnWindowFocus: true, retry: false } },
})

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={client}>
      <PrivacyProvider>
        <App />
      </PrivacyProvider>
    </QueryClientProvider>
  </StrictMode>,
)
