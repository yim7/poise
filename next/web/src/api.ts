// 网页用到的接口（`runtime/src/api.rs`）。
//
// 只走这一层：页面不直接碰交易所，也不自己算交易数量。

import type {
  CanceledOrders,
  DraftIdBody,
  DraftPreview,
  ResumeOutcome,
  StepReport,
  StoredDraft,
  StrategyConfig,
  StrategyDetail,
  StrategyIdBody,
  StrategySummary,
  VersionBody,
} from './types'

export class HttpError extends Error {
  constructor(
    readonly status: number,
    /** 服务端给的机器可读类别；老响应没有这一项时为 null。 */
    readonly kind: string | null,
    message: string,
  ) {
    super(message)
    this.name = 'HttpError'
  }
}

function send(method: string, body?: unknown): RequestInit {
  if (body === undefined) return { method }
  return {
    method,
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, init)
  if (!response.ok) throw await failure(response, path)
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

/** 把出错的响应变成错误对象：状态码、类别、以及服务端那句话。 */
async function failure(response: Response, path: string): Promise<HttpError> {
  let kind: string | null = null
  let message = `${path} ${response.status}`
  try {
    const body = (await response.json()) as { error?: unknown; kind?: unknown }
    if (typeof body.error === 'string') message = body.error
    if (typeof body.kind === 'string') kind = body.kind
  } catch {
    // 没有 JSON 就用状态码，它是确定的信号。
  }
  return new HttpError(response.status, kind, message)
}

export const api = {
  strategies: () => request<StrategySummary[]>('/api/strategies'),
  strategy: (id: string) => request<StrategyDetail>(`/api/strategies/${id}`),
  pause: (id: string) =>
    request<CanceledOrders>(`/api/strategies/${id}/pause`, send('POST')),
  resume: (id: string) => request<ResumeOutcome>(`/api/strategies/${id}/resume`, send('POST')),
  stop: (id: string) => request<CanceledOrders>(`/api/strategies/${id}/stop`, send('POST')),
  updateConfig: (id: string, config: StrategyConfig) =>
    request<VersionBody>(`/api/strategies/${id}/config`, send('PUT', config)),

  drafts: () => request<StoredDraft[]>('/api/drafts'),
  draft: (id: string) => request<StoredDraft>(`/api/drafts/${id}`),
  createDraft: (config: StrategyConfig) => request<DraftIdBody>('/api/drafts', send('POST', config)),
  saveDraft: (id: string, config: StrategyConfig) =>
    request<VersionBody>(`/api/drafts/${id}`, send('PUT', config)),
  deleteDraft: (id: string) => request<void>(`/api/drafts/${id}`, send('DELETE')),
  previewDraft: (id: string) => request<DraftPreview>(`/api/drafts/${id}/preview`, send('POST')),
  startDraft: (id: string, token: string) =>
    request<StrategyIdBody>(`/api/drafts/${id}/start`, send('POST', { token })),
}

/** 恢复的结果里能看出发生了什么，页面只关心「在跑」还是「要等」。 */
export function resumeSummary(outcome: ResumeOutcome): string {
  if (outcome === 'AlreadyRunning') return '策略一直在运行，订单没有动。'
  if ('WaitingForExpiry' in outcome) return `等遗留订单过期后再恢复（${outcome.WaitingForExpiry.until}）。`
  const report: StepReport = outcome.Resumed
  return `已恢复：挂上 ${report.placed.length} 张，撤销 ${report.canceled.length} 张。`
}
