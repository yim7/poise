// 批次表、格子表、挂单表和参数版本表。
//
// 数字全部原样来自服务端：这里只排版，不重算。

import { CELL_STATUS_LABELS, ORDER_STATUS_LABELS, PURPOSE_LABELS, unitLabel } from './labels'
import { decimal, timestamp } from './format'
import type {
  BatchSummary,
  CellSummary,
  ImmediateFill,
  StoredConfigVersion,
  StoredOrder,
} from './types'

/**
 * 批次状态。
 *
 * 服务端给的是「已买入多少、有几个格子已启用、是否结束」，设计第 11 节的四个状态
 * 里有三个能由它们确定：还没放行的、已经放行的、已经结束的。
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
          <th>批次</th>
          <th>批次价</th>
          <th>总量</th>
          <th>主仓</th>
          <th>网格</th>
          <th>已买入</th>
          <th>格子（已启用）</th>
          <th>状态</th>
        </tr>
      </thead>
      <tbody>
        {batches.map((batch) => (
          <tr key={batch.index}>
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
          <th>格子</th>
          <th>所属批次</th>
          <th>买价</th>
          <th>卖价</th>
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
          <tr key={cell.index}>
            <td>{cell.index}</td>
            <td>{cell.batch === null ? '—' : cell.batch}</td>
            <td>{decimal(cell.buy_price)}</td>
            <td>{decimal(cell.sell_price)}</td>
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
            <td>
              {version.config.long.throttle_interval === null
                ? '不限速'
                : `${version.config.long.throttle_interval[0]} 秒`}
            </td>
            <td className="config-summary">
              {version.config.long.price_low}–{version.config.long.price_high}，间距{' '}
              {version.config.long.grid_spacing}，价差 {version.config.long.grid_spread}，上限{' '}
              {version.config.long.max_position}，主仓比例 {version.config.long.core_ratio}
              {version.config.long.batch_count === null
                ? ''
                : `×${version.config.long.batch_count} 批`}
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}
