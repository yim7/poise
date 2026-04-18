const workspaceSections = [
  {
    label: '文件操作区',
    title: '文件操作区',
    description: '导入、保存和会话切换都先放在这里。',
    body: ['导入 Track 文件', '打开最近会话', '保存当前快照'],
  },
  {
    label: 'Track 列表区',
    title: 'Track 列表区',
    description: 'Track 的排序、筛选和当前选择会在这里聚合。',
    body: ['等待载入 Track 数据', '按时间、收益、波动排序', '支持批量选择'],
  },
  {
    label: '关键指标区',
    title: '关键指标区',
    description: '先保留关键指标的概览位，后续再接真实数据。',
    body: ['PnL 概览', '命中率', '回撤与波动'],
  },
  {
    label: '主图区',
    title: '主图区',
    description: '这里承载主图、回放标记和交互反馈。',
    body: ['主图占位', '时间轴回放', '策略标注层'],
  },
  {
    label: '参数编辑区',
    title: '参数编辑区',
    description: '所有调参入口先收敛到这个编辑面板。',
    body: ['参数组', '约束条件', '保存为方案'],
  },
] as const;

function Panel({
  label,
  title,
  description,
  body,
  className,
}: (typeof workspaceSections)[number] & { className: string }) {
  return (
    <section className={`shell-panel ${className}`} aria-label={label}>
      <div className="shell-panel__header">
        <p className="shell-panel__eyebrow">{label}</p>
        <h2 className="shell-panel__title">{title}</h2>
        <p className="shell-panel__description">{description}</p>
      </div>

      <div className="shell-panel__body">
        {body.map((item) => (
          <div className="shell-panel__placeholder" key={item}>
            {item}
          </div>
        ))}
      </div>
    </section>
  );
}

export function AppShell() {
  return (
    <div className="app-shell">
      <header className="app-shell__topbar">
        <div>
          <p className="app-shell__kicker">Poise · Track tuning workbench</p>
          <h1 className="app-shell__headline">Track Tuning Workbench</h1>
        </div>
        <p className="app-shell__summary">
          先搭出清晰的操作骨架，再逐步接入调参、对比和回放能力。
        </p>
      </header>

      <main className="app-shell__workspace" aria-label="Track tuning workspace">
        <Panel {...workspaceSections[0]} className="shell-panel--file" />
        <Panel {...workspaceSections[1]} className="shell-panel--tracks" />
        <Panel {...workspaceSections[2]} className="shell-panel--metrics" />
        <Panel {...workspaceSections[3]} className="shell-panel--chart" />
        <Panel {...workspaceSections[4]} className="shell-panel--params" />
      </main>
    </div>
  );
}
