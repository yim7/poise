// 假的常驻服务：按「方法 + 路径」回答，并记下所有请求。
//
// 前端测试不连真实服务：接口形状用固定数据，测试关心的是页面在什么条件下发什么请求。

import { vi } from 'vitest'

export interface Call {
  method: string
  path: string
  body: unknown
}

export interface Reply {
  status?: number
  body?: unknown
}

type Route = Reply | ((call: Call) => Reply)

export type Routes = Record<string, Route>

export interface FakeServer {
  calls: Call[]
  /** 某个请求发过几次。 */
  count: (method: string, path: string) => number
  /** 最近一次请求的请求体。 */
  lastBody: (method: string, path: string) => unknown
}

export function fakeServer(routes: Routes): FakeServer {
  const calls: Call[] = []
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = String(input)
      const method = init?.method ?? 'GET'
      const body = init?.body === undefined ? undefined : JSON.parse(String(init.body))
      const call: Call = { method, path, body }
      calls.push(call)
      const route = routes[`${method} ${path}`]
      if (route === undefined) throw new Error(`测试没有准备 ${method} ${path}`)
      const reply = typeof route === 'function' ? route(call) : route
      return new Response(reply.body === undefined ? null : JSON.stringify(reply.body), {
        status: reply.status ?? 200,
        headers: { 'content-type': 'application/json' },
      })
    }),
  )
  return {
    calls,
    count: (method, path) => calls.filter((c) => c.method === method && c.path === path).length,
    lastBody: (method, path) => calls.filter((c) => c.method === method && c.path === path).at(-1)?.body,
  }
}
