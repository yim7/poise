// 测试用的固定数据：形状和数量都取自服务端的真实响应（设计第 3.2 节的例子）。

import type {
  BatchSummary,
  CellSummary,
  DraftPreview,
  StoredConfigVersion,
  StoredOrder,
  StrategyConfig,
  StrategyDetail,
  StrategySummary,
} from '../types'

export const config: StrategyConfig = {
  instrument: 'BTC',
  long: {
    price_low: '75000',
    price_high: '95000',
    max_position: '1',
    core_ratio: '0.7',
    batch_count: 4,
    grid_spacing: '1000',
    grid_spread: '1000',
    throttle_interval: null,
  },
}

const batches: BatchSummary[] = [0, 1, 2, 3].map((index) => ({
  index,
  price: String(95000 - index * 5000),
  quantity: '0.25',
  core_quantity: '0.175',
  grid_quantity: '0.075',
  core_filled: '0',
  finished: false,
  cell_count: 5,
  enabled_cell_count: 0,
  core_shortfall: '0',
  grid_shortfall: '0',
}))

const cells: CellSummary[] = [
  {
    index: 0,
    batch: 0,
    buy_price: '95000',
    sell_price: '96000',
    quantity: '0.015',
    held: '0',
    to_buy_back: '0',
    working_orders: 0,
    status: 'waiting_for_batch',
    profit_maker: '14.57025',
    profit_taker: '13.71075',
  },
  {
    index: 1,
    batch: 0,
    buy_price: '94000',
    sell_price: '95000',
    quantity: '0.015',
    held: '0.015',
    to_buy_back: '0.015',
    working_orders: 1,
    status: 'to_buy_back',
    profit_maker: '14.57475',
    profit_taker: '13.72425',
  },
]

const orders: StoredOrder[] = [
  {
    record: {
      client_order_id: '0xabc00000000000000000000000000001',
      unit: { batch: 0 },
      purpose: 'core_buy',
      limit_price: '95000',
      quantity: '0.175',
      cumulative_filled: '0',
      reduce_only: false,
      status: 'open',
    },
    exchange_order_id: 'sim-order-1',
    fills_reconciled: true,
    expires_at: '2026-10-15T00:00:06Z',
    placed_at: '2026-10-15T00:00:01Z',
    updated_at: '2026-10-15T00:00:01Z',
  },
]

const configVersions: StoredConfigVersion[] = [
  { version: 1, config, recorded_at: '2026-10-15T00:00:00Z' },
  {
    version: 2,
    config: { ...config, long: { ...config.long, throttle_interval: [1800, 0] } },
    recorded_at: '2026-10-15T00:10:00Z',
  },
]

export function summary(overrides: Partial<StrategySummary> = {}): StrategySummary {
  return {
    strategy_id: 's-1',
    instrument: 'BTC',
    status: 'running',
    config_version: 2,
    created_at: '2026-10-15T00:00:00Z',
    updated_at: '2026-10-15T00:00:01Z',
    position: '0.015',
    working_orders: 1,
    config,
    ...overrides,
  }
}

export function detail(overrides: Partial<StrategyDetail> = {}): StrategyDetail {
  return {
    summary: summary(),
    taken_at: '2026-10-15T00:00:01Z',
    market_price: '95500',
    available_collateral: '100000',
    position: '0.015',
    expected_position: '0.015',
    note: null,
    batches,
    cells,
    orders,
    waiting: ['waiting_for_batch', 'price_not_reached'],
    config_versions: configVersions,
    ...overrides,
  }
}

export function preview(overrides: Partial<DraftPreview> = {}): DraftPreview {
  return {
    draft_id: 's-1',
    instrument: 'BTC',
    config_version: 1,
    validation: { valid: true, reason: null },
    market_price: '95500',
    position: '0',
    available_collateral: '100000',
    batches,
    cells,
    immediate_fills: [],
    token: 'token-1',
    ...overrides,
  }
}
