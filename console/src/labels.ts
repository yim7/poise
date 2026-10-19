// 界面上的中文说法。服务端的枚举值在这里集中翻译，页面里不出现英文枚举。

import type {
  CellStatus,
  Direction,
  OrderPurpose,
  OrderStatus,
  Side,
  StrategyConfig,
  StrategyStatus,
  Unit,
  WaitReason,
} from './types'

export const STATUS_LABELS: Record<StrategyStatus, string> = {
  draft: '草稿',
  // 恢复中、暂停中、停止中都是过渡状态：后台在撤单和对账，一张新单都不下。
  restoring: '恢复中',
  running: '运行中',
  pausing: '暂停中',
  paused: '已暂停',
  stopping: '停止中',
  // 停止就是归档（第 2.4 节）：命令行和这一页的只读提示都叫「已归档」。
  stopped: '已归档',
  invalidating: '失效中',
  invalidated: '已失效',
  closing: '平仓中',
  closed: '已平仓',
}

/** 归档只读：已停止、已失效、已平仓（第 2.4 节）。标的上还有仓位时只剩市价平仓。 */
export function isArchived(status: StrategyStatus): boolean {
  return status === 'stopped' || status === 'invalidated' || status === 'closed'
}

/**
 * 格子状态只在格子还没启用时显示：等买入、持有、部分持有从持有量一眼就能看出来，
 * 再写一遍是重复。
 */
export const CELL_STATUS_LABELS: Partial<Record<CellStatus, string>> = {
  waiting_for_batch: '等批次',
}

/** 方向的单字简写：格子、批次、挂单归属里用。 */
export const DIRECTION_SHORT: Record<Direction, string> = {
  long: '多',
  short: '空',
}

export const DIRECTION_LABELS: Record<Direction, string> = {
  long: '做多',
  short: '做空',
}

/** 订单用途只说「哪条腿」：开仓、平仓还是主仓。买卖另起一列，按侧推出来。 */
export const PURPOSE_LABELS: Record<OrderPurpose, string> = {
  core: '主仓',
  grid_entry: '开',
  grid_exit: '平',
  invalidation: '失效减仓',
  close: '清仓',
}

/** 订单的买卖：多头侧建仓（主仓、开）是买，平仓、失效减仓和清仓是卖；空头侧反过来。 */
export function orderSide(direction: Direction, purpose: OrderPurpose): Side {
  const opening = purpose === 'core' || purpose === 'grid_entry'
  return opening === (direction === 'long') ? 'buy' : 'sell'
}

/** 订单归属的方向。 */
export function unitDirection(unit: Unit): Direction {
  if ('batch' in unit) return unit.batch[0]
  if ('cell' in unit) return unit.cell[0]
  return unit.side
}

export const SIDE_LABELS: Record<Side, string> = {
  buy: '买',
  sell: '卖',
}

export const ORDER_STATUS_LABELS: Record<OrderStatus, string> = {
  submitting: '待发送',
  open: '在场',
  partially_filled: '部分成交',
  filled: '已成交',
  canceled: '已撤销',
  rejected: '被拒绝',
  void: '作废',
  unknown: '结果待查',
}

/** 等待原因，和设计第 11 节列的一一对应。 */
export const WAIT_REASON_LABELS: Record<WaitReason, string> = {
  price_not_reached: '价格未到',
  waiting_for_throttle: '等待限速放行',
  waiting_for_batch: '等待所属批次',
  limit_adjusted_by_own_order: '限价避让自身挂单',
  insufficient_funds: '资金不足或退避中',
  margin_ratio_above_stop_threshold: '保证金率超过停止阈值',
  margin_ratio_unavailable: '保证金率读不到',
  stale_market_data: '行情过期',
  order_result_unknown: '订单结果待查',
  position_check_failed: '仓位核对失败已暂停',
  invalidated: '观点已失效',
  fills_pending: '成交待对账',
  position_unstable: '仓位变动中',
  fill_records_inconsistent: '成交记录对不上',
}

/** 订单归属：中性网格两侧都有批次和格子，归属里带上方向才分得清。多3 是多头第 3 格，多批0 是多头第 0 批主仓。 */
export function unitLabel(unit: Unit): string {
  if ('batch' in unit) return `${DIRECTION_SHORT[unit.batch[0]]}批${unit.batch[1]}`
  if ('cell' in unit) return `${DIRECTION_SHORT[unit.cell[0]]}${unit.cell[1]}`
  return `${DIRECTION_SHORT[unit.side]}侧`
}

/** 策略类型：只有多头侧是做多，只有空头侧是做空，两侧都有是中性网格。 */
export function kindLabel(config: StrategyConfig): string {
  if (config.long !== null && config.short !== null) return '中性'
  if (config.short !== null) return '做空'
  return '做多'
}

export function statusLabel(status: string): string {
  return STATUS_LABELS[status as StrategyStatus] ?? status
}

export function waitReasonLabel(reason: string): string {
  return WAIT_REASON_LABELS[reason as WaitReason] ?? reason
}
