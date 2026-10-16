// 服务端接口的数据形状（`runtime/src/api.rs`）。
//
// 小数一律是字符串：服务端用的是定点数，前端只负责显示，不重算任何交易数量。

/** 定点小数，原样来自服务端。 */
export type Decimal = string

/**
 * 策略状态。
 *
 * `restoring`／`pausing`／`stopping` 是过渡状态：后台还在撤单、对账，期间一张新单都不下。
 * 它们不是终态，页面要把这件事说清楚，用户才知道还要等。
 */
export type StrategyStatus =
  | 'draft'
  | 'restoring'
  | 'running'
  | 'pausing'
  | 'paused'
  | 'stopping'
  | 'stopped'
  /** 失效中：标记价越过失效价，正在撤单、用只减仓单减到剩余数量（第 7.5 节）。 */
  | 'invalidating'
  /** 已失效：和已归档一样只读；价格回来也不自动恢复。 */
  | 'invalidated'

/** 限速间隔按 `[秒, 纳秒]` 传（chrono 的 Duration）。 */
export type ThrottleInterval = [number, number] | null

/** 网格的一侧：多头侧在价格下方，空头侧在价格上方。 */
export type Direction = 'long' | 'short'

/** 一侧的参数。两侧用同一组字段，方向相反的部分由服务端按 `direction` 算。 */
export interface SideConfig {
  price_low: Decimal
  price_high: Decimal
  max_position: Decimal
  core_ratio: Decimal
  batch_count: number | null
  grid_spacing: Decimal
  grid_spread: Decimal
  throttle_interval: ThrottleInterval
  /** 观点失效（可选，第 7.5 节）；null 表示这一侧不启用自动止损。 */
  invalidation: Invalidation | null
}

/** 观点失效：价格越过失效价时撤单，把本侧仓位减到剩余数量。 */
export interface Invalidation {
  price: Decimal
  /** 失效后剩余数量 R：0 表示全部平仓。 */
  remaining: Decimal
  /** 失效平仓单相对标记价最多让出多少（比率，0.01 是 1%）。 */
  max_slippage: Decimal
}

/**
 * 策略配置：标的与最多两侧。
 *
 * `long` 为 null 表示不做多，`short` 为 null 表示不做空；两侧都不为 null 是中性网格。
 * 两侧都为 null 服务端会拒绝。
 */
export interface StrategyConfig {
  instrument: string
  long: SideConfig | null
  short: SideConfig | null
}

export interface Validation {
  valid: boolean
  reason: string | null
}

export interface BatchSummary {
  /** 属于哪一侧；中性网格两侧都没有主仓，批次表是空的。 */
  direction: Direction
  index: number
  price: Decimal
  quantity: Decimal
  core_quantity: Decimal
  grid_quantity: Decimal
  core_filled: Decimal
  finished: boolean
  cell_count: number
  enabled_cell_count: number
  core_shortfall: Decimal
  grid_shortfall: Decimal
}

export type CellStatus = 'waiting_for_batch' | 'waiting_for_buy' | 'holding' | 'to_buy_back'

export interface CellSummary {
  /** 属于哪一侧。 */
  direction: Direction
  index: number
  batch: number | null
  /** 建仓价：多头侧的买价，空头侧的开空价。 */
  entry_price: Decimal
  /** 平仓价：多头侧的卖价，空头侧的买平价。 */
  exit_price: Decimal
  quantity: Decimal
  held: Decimal
  to_buy_back: Decimal
  working_orders: number
  status: CellStatus
  profit_maker: Decimal
  profit_taker: Decimal
}

/** 订单归属：某一侧的一批主仓或一个格子。 */
export type Unit = { batch: [Direction, number] } | { cell: [Direction, number] }

/** 订单用途：主仓建仓腿、格子的建仓腿和平仓腿。买卖方向由服务端按侧算。 */
export type OrderPurpose = 'core' | 'grid_entry' | 'grid_exit'

export type OrderStatus =
  | 'submitting'
  | 'open'
  | 'partially_filled'
  | 'filled'
  | 'canceled'
  | 'rejected'
  | 'void'
  | 'unknown'

export interface OrderRecord {
  client_order_id: string
  unit: Unit
  purpose: OrderPurpose
  limit_price: Decimal
  quantity: Decimal
  cumulative_filled: Decimal
  reduce_only: boolean
  status: OrderStatus
}

export interface StoredOrder {
  record: OrderRecord
  exchange_order_id: string | null
  fills_reconciled: boolean
  expires_at: string
  placed_at: string
  updated_at: string
}

export interface ImmediateFill {
  unit: Unit
  purpose: OrderPurpose
  limit_price: Decimal
  quantity: Decimal
}

export type Side = 'buy' | 'sell'

/** 启动后第一步会挂出的一张单：限价已经避开本策略自己的反向订单。 */
export interface OrderIntent {
  unit: Unit
  purpose: OrderPurpose
  side: Side
  limit_price: Decimal
  quantity: Decimal
  reduce_only: boolean
}

/** 交易所上的一张挂单。用户手动下的单没有客户端订单 ID。 */
export interface ExchangeOrder {
  client_order_id: string | null
  exchange_order_id: string
  side: Side
  limit_price: Decimal
  quantity: Decimal
  cumulative_filled: Decimal
  reduce_only: boolean
  status: OrderStatus
}

/** 一侧的满仓压力测试（第 7.3 节）。 */
export interface SideStress {
  direction: Direction
  /** 最不利的价格：做多是范围下沿，做空是范围上沿。 */
  adverse_price: Decimal
  position: Decimal
  average_entry: Decimal
  margin_ratio: Decimal
  /** 满仓时会不会强平、在哪里强平。 */
  liquidation: Liquidation
}

/** 满仓时的强平结果：不会强平、在某个价格强平，或者强平价远到算不出来。 */
export type Liquidation =
  | { kind: 'never' }
  | { kind: 'at'; price: Decimal }
  | { kind: 'out_of_range' }

export interface DraftPreview {
  draft_id: string
  instrument: string
  config_version: number
  validation: Validation
  market_price: Decimal
  position: Decimal
  available_collateral: Decimal
  batches: BatchSummary[]
  cells: CellSummary[]
  immediate_fills: ImmediateFill[]
  /** 启动后第一步会挂出的订单；标的上已有仓位时就是接管之后要挂的单（第 8 节）。 */
  planned_orders: OrderIntent[]
  /** 满仓压力测试：每一侧满仓、价格到最不利位置时的保证金率和强平价。 */
  stress: SideStress[]
  /** 满仓保证金率超过提醒阈值时的提醒。 */
  warnings: string[]
  /** 交易所上这个标的已有的挂单：都不是本策略的，本系统不撤销（第 8 节）。 */
  foreign_orders: ExchangeOrder[]
  /** 校验不过时是 null：没有令牌就确认不了。 */
  token: string | null
}

export interface StoredDraft {
  draft_id: string
  config: StrategyConfig
  created_at: string
  updated_at: string
}

export interface StoredConfigVersion {
  version: number
  config: StrategyConfig
  recorded_at: string
}

export interface StrategySummary {
  strategy_id: string
  instrument: string
  status: StrategyStatus
  config_version: number
  created_at: string
  updated_at: string
  position: Decimal
  working_orders: number
  config: StrategyConfig
}

export type WaitReason =
  | 'price_not_reached'
  | 'waiting_for_throttle'
  | 'waiting_for_batch'
  | 'limit_adjusted_by_own_order'
  | 'insufficient_funds'
  | 'margin_ratio_above_stop_threshold'
  | 'margin_ratio_unavailable'
  | 'stale_market_data'
  | 'fill_stream_disconnected'
  | 'order_result_unknown'
  | 'position_check_failed'
  | 'invalidated'
  | 'fills_pending'
  | 'position_unstable'
  | 'fill_records_inconsistent'

export interface StepReport {
  strategy_id: string
  status: StrategyStatus
  placed: string[]
  canceled: string[]
  applied_fills: number
  position: Decimal
  expected_position: Decimal
}

export interface StrategyDetail {
  summary: StrategySummary
  /** 行情、仓位这些读数是什么时候取的：页面据此说明这份读数有多旧。 */
  taken_at: string
  market_price: Decimal
  available_collateral: Decimal
  /** 账户保证金率（第 7.4 节）；那一步没读到时是 null。 */
  margin_ratio: Decimal | null
  position: Decimal
  expected_position: Decimal
  /** 策略为什么还没走完当前状态；没有特殊情况时是 null。 */
  note: string | null
  batches: BatchSummary[]
  cells: CellSummary[]
  orders: StoredOrder[]
  waiting: WaitReason[]
  config_versions: StoredConfigVersion[]
}

/** 恢复的结果：一直在跑、等遗留订单过期、遗留订单没收场、或者真的恢复了。 */
export type ResumeOutcome =
  | 'AlreadyRunning'
  | { WaitingForExpiry: { until: string } }
  | { WaitingForSettle: { working: number } }
  | { Resumed: StepReport }

/**
 * 暂停、停止的结果：撤销了哪些单，以及策略最后落在哪个状态。
 *
 * `status` 不一定就是「已暂停」「已停止」：撤单回执丢了、成交还没对完账时它停在
 * `pausing`／`stopping`，订单还在交易所上，后台每一轮继续撤。页面照它说话。
 */
export interface CanceledOrders {
  canceled: string[]
  status: StrategyStatus
}

export interface VersionBody {
  version: number
}

export interface DraftIdBody {
  draft_id: string
}

export interface StrategyIdBody {
  strategy_id: string
}

/** 账户风控设置（第 7.4、11 节）：比率是小数，例如 0.3 表示 30%。 */
export interface AccountSettings {
  margin_alert_ratio: Decimal
  margin_stop_ratio: Decimal
  /** 行情过期时间，秒。 */
  market_data_max_age: number
}
