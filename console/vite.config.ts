import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// 开发时把 `/api` 转给常驻服务：页面和接口同源，服务端不必开跨域。
// 端口取启动文件里的 `listen`，默认 127.0.0.1:8010。
const api = process.env.POISE_API ?? 'http://127.0.0.1:8010'

export default defineConfig({
  plugins: [react()],
  server: { proxy: { '/api': api } },
  test: { environment: 'jsdom', setupFiles: './src/test/setup.ts' },
})
