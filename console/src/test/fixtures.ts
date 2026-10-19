// 测试用的固定数据：形状和数量都取自服务端的真实响应（设计第 3.2 节的例子）。

import type {
  BatchSummary,
  CellSummary,
  DraftPreview,
  SideConfig,
  StoredConfigVersion,
  StoredOrder,
  StrategyConfig,
  StrategyDetail,
  StrategySummary,
} from '../types'

/** 设计第 3.2 节的例子：做多网格 75000–95000，没有主仓。 */
export const longSide: SideConfig = {
  price_low: '75000',
  price_high: '95000',
  max_position: '1',
  core_ratio: '0',
  batch_count: null,
  grid_spacing: '1000',
  grid_spread: '1000',
  throttle_interval: null,
  invalidation: null,
}

/** 同一份划分、方向相反：做空网格。 */
export const shortSide: SideConfig = { ...longSide }

export const config: StrategyConfig = {
  instrument: 'BTC',
  long: longSide,
  short: null,
}

export const shortConfig: StrategyConfig = {
  instrument: 'BTC',
  long: null,
  short: shortSide,
}

/** 中性网格（第 13 节 B11）：多头侧 79–89，空头侧 91–96，两侧都没有主仓。 */
export const neutralConfig: StrategyConfig = {
  instrument: 'BTC',
  long: {
    price_low: '79',
    price_high: '89',
    max_position: '1',
    core_ratio: '0',
    batch_count: null,
    grid_spacing: '1',
    grid_spread: '1',
    throttle_interval: null,
    invalidation: null,
  },
  short: {
    price_low: '91',
    price_high: '96',
    max_position: '0.25',
    core_ratio: '0',
    batch_count: null,
    grid_spacing: '1',
    grid_spread: '1',
    throttle_interval: null,
    invalidation: null,
  },
}

/** 带主仓的做多侧：`r = .7`、`n = 4`。 */
export const coreSide: SideConfig = {
  ...longSide,
  core_ratio: '0.7',
  batch_count: 4,
}

const batches: BatchSummary[] = [0, 1, 2, 3].map((index) => ({
  direction: 'long' as const,
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
    direction: 'long',
    index: 0,
    batch: 0,
    entry_price: '95000',
    exit_price: '96000',
    quantity: '0.015',
    held: '0',
    to_buy_back: '0',
    working_orders: 0,
    status: 'waiting_for_batch',
    profit_maker: '14.57025',
    profit_taker: '13.71075',
  },
  {
    direction: 'long',
    index: 1,
    batch: 0,
    entry_price: '94000',
    exit_price: '95000',
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
      unit: { batch: ['long', 0] },
      purpose: 'core',
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
    config: { ...config, long: { ...longSide, throttle_interval: [1800, 0] } },
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
    market_price: '95500',
    working_orders: 1,
    config,
    held: null,
    ...overrides,
  }
}

export function detail(overrides: Partial<StrategyDetail> = {}): StrategyDetail {
  return {
    summary: summary(),
    taken_at: '2026-10-15T00:00:01Z',
    market_price: '95500',
    available_collateral: '100000',
    margin_ratio: '0.0123',
    position: '0.015',
    expected_position: '0.015',
    note: null,
    batches,
    cells,
    orders,
    waiting: ['waiting_for_batch', 'price_not_reached'],
    config_versions: configVersions,
    actions_today: 12,
    funding_rate: { hourly: '0.0000125', annual: '0.1095' },
    funding_rate_read_at: '2026-10-15T00:00:00Z',
    funding_total: '-1.5',
    funding_synced_through: '2026-10-15T00:00:00Z',
    grid_realized_profit: '30',
    funding_share: '-0.05',
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
    planned_orders: [],
    stress: [
      {
        direction: 'long',
        adverse_price: '75000',
        position: '1',
        average_entry: '86900',
        margin_ratio: '0.01064131',
        liquidation: { kind: 'never' },
      },
    ],
    warnings: [],
    foreign_orders: [],
    token: 'token-1',
    ...overrides,
  }
}

/** 中性网格的格子：两侧各一格，两侧都没有主仓，所以没有批次。 */
const neutralCells: CellSummary[] = [
  {
    direction: 'long',
    index: 0,
    batch: null,
    entry_price: '89',
    exit_price: '90',
    quantity: '0.1',
    held: '0',
    to_buy_back: '0',
    working_orders: 1,
    status: 'waiting_for_buy',
    profit_maker: '0.097315',
    profit_taker: '0.091945',
  },
  {
    direction: 'short',
    index: 0,
    batch: null,
    entry_price: '91',
    exit_price: '90',
    quantity: '0.05',
    held: '0',
    to_buy_back: '0',
    working_orders: 1,
    status: 'waiting_for_buy',
    profit_maker: '0.0486425',
    profit_taker: '0.0459275',
  },
]

export function neutralPreview(overrides: Partial<DraftPreview> = {}): DraftPreview {
  return preview({ batches: [], cells: neutralCells, ...overrides })
}

/** 做空网格的一格：开空价 96000，买平价 95000。 */
const shortCells: CellSummary[] = [
  {
    direction: 'short',
    index: 0,
    batch: null,
    entry_price: '96000',
    exit_price: '95000',
    quantity: '0.05',
    held: '0',
    to_buy_back: '0',
    working_orders: 1,
    status: 'waiting_for_buy',
    profit_maker: '48.5675',
    profit_taker: '45.7025',
  },
]

export function shortDetail(overrides: Partial<StrategyDetail> = {}): StrategyDetail {
  return detail({
    summary: summary({ config: shortConfig, position: '0', working_orders: 1 }),
    batches: [],
    cells: shortCells,
    position: '0',
    expected_position: '0',
    waiting: [],
    ...overrides,
  })
}

export function neutralDetail(overrides: Partial<StrategyDetail> = {}): StrategyDetail {
  return detail({
    summary: summary({ config: neutralConfig, position: '0', working_orders: 2 }),
    batches: [],
    cells: neutralCells,
    position: '0',
    expected_position: '0',
    waiting: [],
    ...overrides,
  })
}
