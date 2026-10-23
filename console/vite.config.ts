import path from 'node:path'

import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// 开发时把 `/api` 转给常驻服务：页面和接口同源，服务端不必开跨域。
// 端口取启动文件里的 `listen`，默认 127.0.0.1:8010。
const api = process.env.POISE_API ?? 'http://127.0.0.1:8010'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { '@': path.resolve(import.meta.dirname, './src') } },
  server: {
    proxy: { '/api': api },
    // 和常驻服务一样不许别的页面嵌入：开发页面也能经代理暂停、停止、平仓。
    headers: { 'Content-Security-Policy': "frame-ancestors 'none'", 'X-Frame-Options': 'DENY' },
  },
  test: { environment: 'jsdom', setupFiles: './src/test/setup.ts' },
})
