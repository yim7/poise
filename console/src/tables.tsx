// 批次表、格子表、挂单表和参数版本表。
//
// 数字全部原样来自服务端：这里只排版，不重算。

import {
  CELL_STATUS_LABELS,
  DIRECTION_LABELS,
  ORDER_STATUS_LABELS,
  PURPOSE_LABELS,
  SIDE_LABELS,
  unitLabel,
} from './labels'
import { configSummary, decimal, percent, timestamp } from './format'
import type {
  BatchSummary,
  CellSummary,
  ExchangeOrder,
  ImmediateFill,
  Liquidation,
  OrderIntent,
  SideStress,
  StoredConfigVersion,
  StoredOrder,
  StrategyConfig,
  ThrottleInterval,
} from './types'

/**
 * 批次状态：还没放行的、已经放行的、已经结束的（设计第 11 节）。
 *
 * 服务端给的是「已买入多少、有几个格子已启用、是否结束」，状态由它们确定。
 */
function batchState(batch: BatchSummary): string {
  if (batch.finished) return '已结束'
  if (batch.enabled_cell_count > 0 || Number(batch.core_filled) > 0) return '已放行'
  return '未放行'
}

export function BatchTable({ batches }: { batches: BatchSummary[] }) {
  return (
    <table>
      <caption>主仓批次表</caption>
      <thead>
        <tr>
          <th>方向</th>
          <th>批次</th>
          <th>批次价</th>
          <th>总量</th>
          <th>主仓</th>
          <th>网格</th>
          <th>已建仓</th>
          <th>格子（已启用）</th>
          <th>状态</th>
          <th>少建（主仓 / 网格）</th>
        </tr>
      </thead>
      <tbody>
        {batches.map((batch) => (
          <tr key={`${batch.direction}-${batch.index}`}>
            <td>{DIRECTION_LABELS[batch.direction]}</td>
            <td>{batch.index}</td>
            <td>{decimal(batch.price)}</td>
            <td>{decimal(batch.quantity)}</td>
            <td>{decimal(batch.core_quantity)}</td>
            <td>{decimal(batch.grid_quantity)}</td>
            <td>{decimal(batch.core_filled)}</td>
            <td>
              {batch.cell_count}（{batch.enabled_cell_count}）
            </td>
            <td>{batchState(batch)}</td>
            {/* 剩余不足最低下单金额而提前结束的批次，少建了多少主仓和网格（第 4.4 节，B05）。 */}
            <td>
              {Number(batch.core_shortfall) === 0 && Number(batch.grid_shortfall) === 0
                ? '—'
                : `${decimal(batch.core_shortfall)} / ${decimal(batch.grid_shortfall)}`}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

export function CellTable({ cells }: { cells: CellSummary[] }) {
  return (
    <table>
      <caption>网格格子表</caption>
      <thead>
        <tr>
          <th>方向</th>
          <th>格子</th>
          <th>所属批次</th>
          <th>建仓价</th>
          <th>平仓价</th>
          <th>数量</th>
          <th>持有</th>
          <th>待买回</th>
          <th>在场订单</th>
          <th>状态</th>
          <th>整格收益（maker）</th>
          <th>整格收益（taker）</th>
        </tr>
      </thead>
      <tbody>
        {cells.map((cell) => (
          <tr key={`${cell.direction}-${cell.index}`}>
            <td>{DIRECTION_LABELS[cell.direction]}</td>
            <td>{cell.index}</td>
            <td>{cell.batch === null ? '—' : cell.batch}</td>
            <td>{decimal(cell.entry_price)}</td>
            <td>{decimal(cell.exit_price)}</td>
            <td>{decimal(cell.quantity)}</td>
            <td>{decimal(cell.held)}</td>
            <td>{decimal(cell.to_buy_back)}</td>
            <td>{cell.working_orders}</td>
            <td>{CELL_STATUS_LABELS[cell.status]}</td>
            <td>{decimal(cell.profit_maker)}</td>
            <td>{decimal(cell.profit_taker)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

export function OrdersTable({ orders }: { orders: StoredOrder[] }) {
  if (orders.length === 0) return <p>没有在场挂单。</p>
  return (
    <table>
      <caption>在场挂单</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th>用途</th>
          <th>限价</th>
          <th>数量</th>
          <th>已成交</th>
          <th>状态</th>
          <th>到期</th>
        </tr>
      </thead>
      <tbody>
        {orders.map((order) => (
          <tr key={order.record.client_order_id}>
            <td>{unitLabel(order.record.unit)}</td>
            <td>{PURPOSE_LABELS[order.record.purpose]}</td>
            <td>{decimal(order.record.limit_price)}</td>
            <td>{decimal(order.record.quantity)}</td>
            <td>{decimal(order.record.cumulative_filled)}</td>
            <td>{ORDER_STATUS_LABELS[order.record.status]}</td>
            <td>{timestamp(order.expires_at)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

/** 启动后会立即成交的订单：确认启动之前必须看到这些。 */
export function ImmediateFillTable({ fills }: { fills: ImmediateFill[] }) {
  if (fills.length === 0) return <p>启动时没有会立即成交的订单。</p>
  return (
    <table>
      <caption>启动后立即成交</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th>用途</th>
          <th>限价</th>
          <th>数量</th>
        </tr>
      </thead>
      <tbody>
        {fills.map((fill, index) => (
          <tr key={`${unitLabel(fill.unit)}-${fill.purpose}-${index}`}>
            <td>{unitLabel(fill.unit)}</td>
            <td>{PURPOSE_LABELS[fill.purpose]}</td>
            <td>{decimal(fill.limit_price)}</td>
            <td>{decimal(fill.quantity)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

/** 启动后第一步会挂出的订单（第 8 节：接管已有仓位时，持有格子的平仓单和没买完的主仓单）。 */
export function PlannedOrderTable({ orders }: { orders: OrderIntent[] }) {
  if (orders.length === 0) return <p>启动后第一步不会挂出订单。</p>
  return (
    <table>
      <caption>启动后第一步挂出的订单</caption>
      <thead>
        <tr>
          <th>归属</th>
          <th>用途</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
          <th>只减仓</th>
        </tr>
      </thead>
      <tbody>
        {orders.map((order, index) => (
          <tr key={`${unitLabel(order.unit)}-${order.purpose}-${index}`}>
            <td>{unitLabel(order.unit)}</td>
            <td>{PURPOSE_LABELS[order.purpose]}</td>
            <td>{SIDE_LABELS[order.side]}</td>
            <td>{decimal(order.limit_price)}</td>
            <td>{decimal(order.quantity)}</td>
            <td>{order.reduce_only ? '是' : '否'}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

/** 交易所上这个标的已有的挂单：不是本策略的，本系统不撤销，由用户自己处理（第 8 节）。 */
export function ForeignOrderTable({ orders }: { orders: ExchangeOrder[] }) {
  if (orders.length === 0) return null
  return (
    <table>
      <caption>交易所上已有的其他挂单（本系统不撤销）</caption>
      <thead>
        <tr>
          <th>订单</th>
          <th>买卖</th>
          <th>限价</th>
          <th>数量</th>
          <th>已成交</th>
        </tr>
      </thead>
      <tbody>
        {orders.map((order) => (
          <tr key={order.exchange_order_id}>
            <td>{order.client_order_id ?? `${order.exchange_order_id}（手动下单）`}</td>
            <td>{SIDE_LABELS[order.side]}</td>
            <td>{decimal(order.limit_price)}</td>
            <td>{decimal(order.quantity)}</td>
            <td>{decimal(order.cumulative_filled)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function liquidationText(liquidation: Liquidation): string {
  switch (liquidation.kind) {
    case 'never':
      return '不会强平'
    case 'at':
      return decimal(liquidation.price)
    case 'out_of_range':
      return '超出计算范围'
  }
}

/** 满仓压力测试（第 7.3 节）：每一侧满仓、价格到最不利位置时的账户保证金率和强平价。 */
export function StressTable({ sides }: { sides: SideStress[] }) {
  if (sides.length === 0) return null
  return (
    <table>
      <caption>满仓压力测试</caption>
      <thead>
        <tr>
          <th>方向</th>
          <th>最不利价格</th>
          <th>满仓数量</th>
          <th>持仓均价</th>
          <th>预计保证金率</th>
          <th>强平价</th>
        </tr>
      </thead>
      <tbody>
        {sides.map((side) => (
          <tr key={side.direction}>
            <td>{DIRECTION_LABELS[side.direction]}</td>
            <td>{decimal(side.adverse_price)}</td>
            <td>{decimal(side.position)}</td>
            <td>{decimal(side.average_entry)}</td>
            <td>{percent(side.margin_ratio)}</td>
            <td>{liquidationText(side.liquidation)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function intervalText(interval: ThrottleInterval): string {
  return interval === null ? '不限速' : `${interval[0]} 秒`
}

/** 每一侧的限速间隔各说一遍：中性网格两侧各有一条限速链。 */
function throttleText(config: StrategyConfig): string {
  const parts: string[] = []
  if (config.long !== null) parts.push(`做多 ${intervalText(config.long.throttle_interval)}`)
  if (config.short !== null) parts.push(`做空 ${intervalText(config.short.throttle_interval)}`)
  return parts.join('；')
}

export function ConfigVersionTable({ versions }: { versions: StoredConfigVersion[] }) {
  return (
    <table>
      <caption>参数版本</caption>
      <thead>
        <tr>
          <th>版本</th>
          <th>记录时间</th>
          <th>限速间隔</th>
          <th>参数</th>
        </tr>
      </thead>
      <tbody>
        {versions.map((version) => (
          <tr key={version.version}>
            <td>{version.version}</td>
            <td>{timestamp(version.recorded_at)}</td>
            <td>{throttleText(version.config)}</td>
            <td className="config-summary">{configSummary(version.config)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
