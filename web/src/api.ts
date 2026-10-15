export class HttpResponseError extends Error {
  constructor(readonly status: number, message: string) { super(message) }
}

export async function requestJson<T>(path: string, init?: RequestInit) {
  const response = init ? await fetch(path, init) : await fetch(path)
  if (!response.ok) {
    let message = `${path} ${response.status}`
    try {
      const body = await response.json() as { error?: unknown; message?: unknown }
      const detail = typeof body.error === 'string' ? body.error : typeof body.message === 'string' ? body.message : null
      if (detail) message = detail
    } catch { /* Keep the HTTP status as the deterministic error signal. */ }
    throw new HttpResponseError(response.status, message)
  }
  return response.json() as Promise<T>
}
