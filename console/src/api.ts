// 网页用到的接口（`runtime/src/api.rs`）。
//
// 只走这一层：页面不直接碰交易所，也不自己算交易数量。

import type {
  AccountSettings,
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

  accountSettings: () => request<AccountSettings>('/api/account/settings'),
  updateAccountSettings: (settings: AccountSettings) =>
    request<AccountSettings>('/api/account/settings', send('PUT', settings)),
}

/** 恢复的结果里能看出发生了什么，页面只关心「在跑」还是「要等」。 */
export function resumeSummary(outcome: ResumeOutcome): string {
  if (outcome === 'AlreadyRunning') return '策略一直在运行，订单没有动。'
  if ('WaitingForExpiry' in outcome) return `等遗留订单过期后再恢复（${outcome.WaitingForExpiry.until}）。`
  // 「没收场」不等于「没撤掉」：结果从来没拿到过的订单，撤销只会得到「查无此单」，
  // 后台核对也问不出结局。说成「没撤掉、会继续撤」，用户会以为等下去就能好。
  if ('WaitingForSettle' in outcome)
    return `还有 ${outcome.WaitingForSettle.working} 张遗留订单没收场，后台会继续撤单和核对，收干净才恢复运行。`
  const report: StepReport = outcome.Resumed
  return `已恢复：挂上 ${report.placed.length} 张，撤销 ${report.canceled.length} 张。`
}

/**
 * 暂停的结果：只有接口确认落在「已暂停」才说暂停完成。
 *
 * 「暂停中」不是同一个状态：撤单回执丢了、成交还没对完账的时候订单还在交易所上，
 * 后台每一轮继续撤。说「已暂停」用户就以为挂单已经撤掉了（第 2.4 节）。
 */
export function pauseSummary(canceled: CanceledOrders): string {
  if (canceled.status === 'paused') return `已暂停，撤销 ${canceled.canceled.length} 张订单。`
  return `正在撤单：已撤销 ${canceled.canceled.length} 张订单，还有挂单在场，后台会继续撤。`
}

/** 停止的结果：和暂停一样，只有接口确认落在「已停止」才说归档完成。 */
export function stopSummary(canceled: CanceledOrders): string {
  if (canceled.status === 'stopped')
    return `已停止并归档，撤销 ${canceled.canceled.length} 张订单；仓位保留。`
  return `正在撤单：已撤销 ${canceled.canceled.length} 张订单，还有挂单在场，撤干净才归档；仓位保留。`
}
