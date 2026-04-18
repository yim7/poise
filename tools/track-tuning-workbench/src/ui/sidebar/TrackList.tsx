import { StatusBadge } from '@/ui/common/StatusBadge';

export interface TrackListItem {
  draftId: string;
  trackId: string;
  symbol: string;
  isSelected: boolean;
  isDirty: boolean;
  hasErrors: boolean;
}

export interface TrackListProps {
  items: TrackListItem[];
  selectedTrackId: string | null;
  onSelect(draftId: string): void;
  onAddBlank(): void;
  onDuplicateSelected(): void;
  onDelete(draftId: string): void;
}

export function TrackList({
  items,
  selectedTrackId,
  onSelect,
  onAddBlank,
  onDuplicateSelected,
  onDelete,
}: TrackListProps) {
  return (
    <section className="workbench-panel workbench-panel--sidebar" aria-label="Track 列表区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">Track 管理</p>
          <h2 className="workbench-panel__title">本地工作集</h2>
        </div>
        <StatusBadge tone="neutral">{items.length} 条</StatusBadge>
      </div>

      <div className="track-list__toolbar">
        <button className="button button--secondary" type="button" onClick={onAddBlank}>
          空白新建
        </button>
        <button
          className="button button--secondary"
          type="button"
          onClick={onDuplicateSelected}
          disabled={!selectedTrackId}
        >
          复制草稿
        </button>
      </div>

      {items.length === 0 ? (
        <div className="empty-state">
          <p className="empty-state__title">还没有 Track 草稿</p>
          <p className="empty-state__body">先选择配置文件，或从这里新增一个空白 Track。</p>
        </div>
      ) : (
        <ul className="track-list" role="list">
          {items.map((item) => {
            const itemClassName = [
              'track-list__item',
              item.isSelected ? 'track-list__item--selected' : '',
              item.isDirty ? 'track-list__item--dirty' : '',
              item.hasErrors ? 'track-list__item--error' : '',
            ]
              .filter(Boolean)
              .join(' ');

            return (
              <li className={itemClassName} key={item.draftId}>
                <button
                  className="track-list__select"
                  type="button"
                  onClick={() => onSelect(item.draftId)}
                >
                  <span className="track-list__identity">{item.trackId}</span>
                  <span className="track-list__symbol">{item.symbol}</span>
                </button>

                <div className="track-list__meta">
                  {item.isSelected ? <StatusBadge tone="accent">当前</StatusBadge> : null}
                  {item.isDirty ? <StatusBadge tone="warning">已修改</StatusBadge> : null}
                  {item.hasErrors ? <StatusBadge tone="danger">待修正</StatusBadge> : null}
                </div>

                <div className="track-list__item-actions">
                  <button
                    className="icon-button"
                    type="button"
                    onClick={() => onDelete(item.draftId)}
                    aria-label={`删除 Track ${item.trackId}`}
                  >
                    删除
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
