import { StatusBadge } from '@/ui/common/StatusBadge';

export interface FilePanelProps {
  currentFilePath: string | null;
  dirty: boolean;
  canUndo: boolean;
  canRedo: boolean;
  hasDrafts: boolean;
  hasSelection: boolean;
  onChooseFile(): void;
  onUndo(): void;
  onRedo(): void;
  onCopyCurrent(): void;
  onCopyAll(): void;
}

export function FilePanel({
  currentFilePath,
  dirty,
  canUndo,
  canRedo,
  hasDrafts,
  hasSelection,
  onChooseFile,
  onUndo,
  onRedo,
  onCopyCurrent,
  onCopyAll,
}: FilePanelProps) {
  return (
    <section className="workbench-panel workbench-panel--sidebar" aria-label="文件操作区">
      <div className="workbench-panel__header">
        <div>
          <p className="workbench-panel__eyebrow">文件与导出</p>
          <h2 className="workbench-panel__title">当前草稿</h2>
        </div>
        <StatusBadge tone={dirty ? 'warning' : 'success'}>
          {dirty ? '草稿未导出' : '已与源快照对齐'}
        </StatusBadge>
      </div>

      <div className="file-panel__path-block">
        <p className="file-panel__path-label">配置文件</p>
        <p className="file-panel__path-value">
          {currentFilePath ?? '尚未选择配置文件'}
        </p>
      </div>

      <div className="file-panel__actions">
        <button className="button button--primary" type="button" onClick={onChooseFile}>
          选择配置文件
        </button>
        <button
          className="button button--secondary"
          type="button"
          onClick={onUndo}
          disabled={!canUndo}
        >
          撤销
        </button>
        <button
          className="button button--secondary"
          type="button"
          onClick={onRedo}
          disabled={!canRedo}
        >
          重做
        </button>
        <button
          className="button button--ghost"
          type="button"
          onClick={onCopyCurrent}
          disabled={!hasSelection}
        >
          复制当前 Track
        </button>
        <button
          className="button button--ghost"
          type="button"
          onClick={onCopyAll}
          disabled={!hasDrafts}
        >
          复制全部 Tracks
        </button>
      </div>

      <div className="file-panel__footer">
        <StatusBadge tone="accent">Binance 待接通</StatusBadge>
        <p className="file-panel__note">
          当前价格与复制命令会在 Task 7 接真实命令层；这一轮先把工作台结构和交互边界钉住。
        </p>
      </div>
    </section>
  );
}
