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
  server: { proxy: { '/api': api } },
  test: { environment: 'jsdom', setupFiles: './src/test/setup.ts' },
})
