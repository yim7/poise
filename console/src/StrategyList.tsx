// 策略列表（设计第 11 节）：标的、类型、状态、净仓位、在场挂单，外加未开始的草稿。
//
// 列表只读服务端算好的汇总，不再去交易所取数：行情和等待原因在详情里看。

import { useQuery } from '@tanstack/react-query'

import { api } from './api'
import { configSummary, decimal, timestamp } from './format'
import { kindLabel, statusLabel } from './labels'

const REFRESH_MS = 5000

interface StrategyListProps {
  onOpenStrategy: (id: string) => void
  onOpenDraft: (id: string) => void
  onNew: () => void
  onOpenSettings: () => void
}

export function StrategyList({
  onOpenStrategy,
  onOpenDraft,
  onNew,
  onOpenSettings,
}: StrategyListProps) {
  const strategies = useQuery({
    queryKey: ['strategies'],
    queryFn: api.strategies,
    refetchInterval: REFRESH_MS,
  })
  const drafts = useQuery({
    queryKey: ['drafts'],
    queryFn: api.drafts,
    refetchInterval: REFRESH_MS,
  })

  const failure = strategies.error ?? drafts.error

  return (
    <section className="list">
      <div className="actions">
        <button type="button" className="primary" onClick={onNew}>
          新建策略
        </button>
        <button type="button" onClick={onOpenSettings}>
          账户风控设置
        </button>
      </div>
      {failure === null || failure === undefined ? null : (
        <p className="error">读取失败：{failure.message}</p>
      )}

      <h2>策略</h2>
      {strategies.data === undefined ? (
        <p>读取中…</p>
      ) : strategies.data.length === 0 ? (
        <p>还没有策略。</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>标的</th>
              <th>类型</th>
              <th>状态</th>
              <th>净仓位</th>
              <th>在场挂单</th>
              <th>参数版本</th>
              <th>更新时间</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {strategies.data.map((strategy) => (
              <tr key={strategy.strategy_id}>
                <td>{strategy.instrument}</td>
                <td>{kindLabel(strategy.config)}</td>
                <td>{statusLabel(strategy.status)}</td>
                <td>{decimal(strategy.position)}</td>
                <td>{strategy.working_orders}</td>
                <td>{strategy.config_version}</td>
                <td>{timestamp(strategy.updated_at)}</td>
                <td>
                  <button type="button" onClick={() => onOpenStrategy(strategy.strategy_id)}>
                    查看
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>草稿</h2>
      {drafts.data === undefined ? (
        <p>读取中…</p>
      ) : drafts.data.length === 0 ? (
        <p>没有未开始的草稿。</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>标的</th>
              <th>类型</th>
              <th>状态</th>
              <th>参数</th>
              <th>更新时间</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {drafts.data.map((draft) => (
              <tr key={draft.draft_id}>
                <td>{draft.config.instrument}</td>
                <td>{kindLabel(draft.config)}</td>
                <td>草稿</td>
                <td className="config-summary">{configSummary(draft.config)}</td>
                <td>{timestamp(draft.updated_at)}</td>
                <td>
                  <button type="button" onClick={() => onOpenDraft(draft.draft_id)}>
                    编辑
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}
