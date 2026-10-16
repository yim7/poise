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
}

export const CELL_STATUS_LABELS: Record<CellStatus, string> = {
  waiting_for_batch: '等待所属批次',
  waiting_for_buy: '等待买入',
  holding: '持有中',
  to_buy_back: '等待买回',
}

export const DIRECTION_LABELS: Record<Direction, string> = {
  long: '做多',
  short: '做空',
}

/** 订单用途只说「哪条腿」：买卖方向按侧相反，写「买入／卖出」在空头侧就是错的。 */
export const PURPOSE_LABELS: Record<OrderPurpose, string> = {
  core: '主仓建仓',
  grid_entry: '格子建仓',
  grid_exit: '格子平仓',
}

export const SIDE_LABELS: Record<Side, string> = {
  buy: '买',
  sell: '卖',
}

export const ORDER_STATUS_LABELS: Record<OrderStatus, string> = {
  submitting: '已记录待发送',
  open: '在场',
  partially_filled: '部分成交',
  filled: '已成交',
  canceled: '已撤销',
  rejected: '被拒绝',
  void: '作废（核对为没有影响）',
  unknown: '结果待查',
}

/** 设计第 11 节列的等待原因，加上实现里新增的两条。 */
export const WAIT_REASON_LABELS: Record<WaitReason, string> = {
  price_not_reached: '价格未到',
  waiting_for_throttle: '等待限速放行',
  waiting_for_batch: '等待所属批次',
  limit_adjusted_by_own_order: '限价因自身反向订单下调',
  insufficient_funds: '资金不足或退避中',
  margin_ratio_above_stop_threshold: '保证金率超过停止阈值',
  stale_market_data: '行情过期',
  fill_stream_disconnected: '成交推送断开',
  order_result_unknown: '订单结果待查',
  position_check_failed: '仓位核对失败已暂停',
  invalidated: '观点已失效',
  fills_pending: '成交待对账',
  position_unstable: '仓位数据还在变化',
  fill_records_inconsistent: '成交记录对不上',
}

/** 订单归属：中性网格两侧都有批次和格子，归属里必须带上方向才分得清。 */
export function unitLabel(unit: Unit): string {
  return 'batch' in unit
    ? `${DIRECTION_LABELS[unit.batch[0]]}第 ${unit.batch[1]} 批主仓`
    : `${DIRECTION_LABELS[unit.cell[0]]}第 ${unit.cell[1]} 格`
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
