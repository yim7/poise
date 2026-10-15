import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { TrackCreationPanel } from './TrackCreationPanel'

const context = { version: 'file-1', pending: [] }
const preview = {
  preview_token: 'proof-1', previewed_at: '2026-10-01T00:00:00Z', mark_price: 100,
  position_quantity: 3, quantity_unit: 'GOLD', notional_asset: 'USDC', settlement_asset: 'USDC',
  policy_target_quantity: 1, initial_execution_quantity: 1,
  available_increase_notional: 1000, required_increase_notional: 200,
  orders_to_cancel_on_restart: [{ order_id: 'gold-order', side: 'sell', price: 110, quantity: 0.5 }],
}
function response(value: unknown, status = 200) { return new Response(JSON.stringify(value), { status }) }
function fill() {
  for (const [label, value] of Object.entries({ 'Track ID': 'gold', '合约名称': 'xyz:GOLD', '下限价格': '90', '上限价格': '110', '多头 exposure': '2', '每单位名义金额': '100', '最大名义金额': '200', '每日损失限额': '50', '总损失限额': '150', '杠杆': '2' })) {
    fireEvent.change(screen.getByLabelText(label), { target: { value } })
  }
}
describe('create a track for restart', () => {
  afterEach(() => { cleanup(); vi.unstubAllGlobals() })
  beforeEach(() => {
    vi.stubGlobal('fetch', vi.fn((path: string) => Promise.resolve(response(path === '/track-creation' ? context : preview))))
  })
  it('previews the actual position and orders before explicit save, then displays pending restart', async () => {
    const saved = vi.fn()
    render(<TrackCreationPanel exchange="hyperliquid" deployment="mainnet" onSaved={saved} />)
    await screen.findByLabelText('Track ID'); fill()
    expect(screen.getByRole('button', { name: '保存，重启后生效' })).toBeDisabled()
    fireEvent.click(screen.getByRole('button', { name: '预览接管影响' }))
    expect(await screen.findByText('3.0000 GOLD')).toBeVisible()
    expect(screen.getByText('gold-order')).toBeVisible()
    expect(screen.getByText(/可能减仓/)).toBeVisible()
    expect(vi.mocked(fetch).mock.calls.filter(([path]) => path === '/tracks')).toHaveLength(0)
    fireEvent.click(screen.getByRole('button', { name: '保存，重启后生效' }))
    const dialog = screen.getByRole('dialog', { name: '确认新增 Track' })
    expect(within(dialog).getByText(/重启时会撤销/)).toBeVisible()
    vi.mocked(fetch).mockImplementation(path => Promise.resolve(response(path === '/tracks' ? { version: 'file-2', pending: [{ track_id: 'gold', symbol: 'xyz:GOLD' }] } : preview)))
    fireEvent.click(within(dialog).getByRole('button', { name: '确认保存' }))
    expect(await screen.findByRole('heading', { name: '已保存，等待重启' })).toBeVisible()
    expect(saved).toHaveBeenCalled()
    const [, init] = vi.mocked(fetch).mock.calls.find(([path]) => path === '/tracks')!
    expect(JSON.parse(String(init?.body))).toMatchObject({ expected_version: 'file-1', preview_token: 'proof-1', candidate: { track_id: 'gold', symbol: 'xyz:GOLD', leverage: 2 } })
  })
  it('invalidates preview on edits and ignores a late response to an old draft', async () => {
    let resolve!: (value: Response) => void
    vi.mocked(fetch).mockImplementation(path => path === '/track-creation' ? Promise.resolve(response(context)) : new Promise(value => { resolve = value }))
    render(<TrackCreationPanel exchange="hyperliquid" deployment="testnet" onSaved={() => {}} />)
    await screen.findByLabelText('Track ID'); fill()
    fireEvent.click(screen.getByRole('button', { name: '预览接管影响' }))
    await waitFor(() => expect(resolve).toBeDefined())
    fireEvent.change(screen.getByLabelText('多头 exposure'), { target: { value: '3' } })
    resolve(response(preview))
    await waitFor(() => expect(screen.getByRole('button', { name: '保存，重启后生效' })).toBeDisabled())
    expect(screen.queryByText('gold-order')).not.toBeInTheDocument()
  })
  it('shows save conflicts and requires a fresh preview without resubmitting automatically', async () => {
    render(<TrackCreationPanel exchange="hyperliquid" deployment="testnet" onSaved={() => {}} />)
    await screen.findByLabelText('Track ID'); fill()
    fireEvent.click(screen.getByRole('button', { name: '预览接管影响' }))
    await screen.findByText('gold-order')
    fireEvent.click(screen.getByRole('button', { name: '保存，重启后生效' }))
    vi.mocked(fetch).mockImplementation(path => Promise.resolve(response(path === '/tracks' ? { error: '仓位或挂单已变化，请重新预览' } : context, path === '/tracks' ? 409 : 200)))
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: '确认保存' }))
    expect(await screen.findByText('仓位或挂单已变化，请重新预览')).toBeVisible()
    expect(screen.getByRole('button', { name: '保存，重启后生效' })).toBeDisabled()
    expect(vi.mocked(fetch).mock.calls.filter(([path]) => path === '/tracks')).toHaveLength(1)
  })

  it('recovers a lost save response from persisted pending state regardless of JSON key order', async () => {
    let receipt: object = context
    vi.mocked(fetch).mockImplementation((path, init) => {
      if (path === '/tracks') {
        const candidate = JSON.parse(String(init?.body)).candidate
        receipt = { version: 'file-2', pending: [{ configuration: Object.fromEntries(Object.entries(candidate.configuration).reverse()), tick_timeout_secs: candidate.tick_timeout_secs, leverage: candidate.leverage, symbol: candidate.symbol, track_id: candidate.track_id }] }
        return Promise.reject(new Error('response lost'))
      }
      return Promise.resolve(response(path === '/track-creation' ? receipt : preview))
    })
    render(<TrackCreationPanel exchange="hyperliquid" deployment="testnet" onSaved={() => {}} />)
    await screen.findByLabelText('Track ID'); fill()
    fireEvent.click(screen.getByRole('button', { name: '预览接管影响' }))
    await screen.findByText('gold-order')
    fireEvent.click(screen.getByRole('button', { name: '保存，重启后生效' }))
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: '确认保存' }))
    expect(await screen.findByRole('heading', { name: '已保存，等待重启' })).toBeVisible()
    expect(vi.mocked(fetch).mock.calls.filter(([path]) => path === '/tracks')).toHaveLength(1)
  })
})
