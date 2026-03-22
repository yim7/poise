# TODO

当前 TODO 只跟踪最近 1 到 3 个里程碑的可执行任务。
阶段目标与顺序见 [`docs/roadmap.md`](docs/roadmap.md)，近期策略与验收标准见 [`docs/plan.md`](docs/plan.md)。

## 当前状态

- 当前主线：`K10` 单实例 `7x24` 值守硬化
- 并行线程：同一环境多实例支持正在推进；本主线只要求“实例感知，单实例验收”
- 最近完成：`K9` 主网运行安全底座已实现并验收通过；`K8` TUI 中英文切换；`K7` Web UI 查询模型与能力边界预备；`K6` replay / paper / testnet 验证；`K5` 网格策略与风控；`K4` 执行闭环与命令语义做实
- 最近验证：`2026-03-22` 已通过 `cargo test -p grid-platform-service --lib`、`cargo test -p grid-platform-service --test cli -- --nocapture`、`cargo test -p grid-platform-service --test mainnet_bootstrap -- --nocapture`、`cargo test -p grid-platform-service --test control_plane -- --nocapture`、`cargo test -p grid-platform-service --test persistence_recovery -- --nocapture`、`cargo test -p grid-platform-service --test kernel_flow -- --nocapture`、`cargo test -p grid-platform-tui --test local_paper_e2e` 与 `cargo test`

## K9 主网运行安全底座

### K9.1 测试先补齐

- [x] 为 `paper / testnet / mainnet` 运行级别解析与显式开启规则补测试
- [x] 为启动前交易所快照收集补测试
- [x] 为启动对账补测试，覆盖“继续运行 / 暂停待确认”语义
- [x] 为稳定原因码持久化与查询暴露补测试
- [x] 为硬保护命中后的自动暂停补测试

### K9.2 运行级别与准入保护

- [x] 明确 `paper / testnet / mainnet` 运行级别模型
- [x] 增加 mainnet 显式开启入口，默认不能误入
- [x] 建立实例级默认 SQLite 路径推导

### K9.3 启动对账与自动暂停

- [x] 建立 Binance 启动前签名持仓 / 挂单快照收集
- [x] 建立启动对账流程
- [x] 让 mainnet 缺少签名状态时拒绝启动
- [x] 让持仓或交易所挂单不一致默认进入暂停态
- [x] 为启动暂停和运行期保护写入稳定原因码
- [x] 让 breaker 命中和连续策略同步失败自动暂停策略

### K9.4 事件、日志与实例边界

- [x] 为启动暂停和运行期保护补 `SystemEvent.code`
- [x] 为 `SystemEvent.code` 补 SQLite 持久化
- [x] 为 `/query/alerts` 暴露系统事件原因码
- [x] 为默认数据路径保留实例边界

### K9.5 收尾与验收

- [x] 跑通 K9 对应测试矩阵
- [x] 复核 K9 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- [x] 记录最近一次验证结果
- [x] 验收结论：K9 已验收通过

## K10 单实例 `7x24` 值守硬化

- [ ] 固化单实例部署、工作目录、数据目录与日志目录约定
- [ ] 增加重复启动保护与优雅停机流程
- [ ] 抽出统一健康状态与自动降级语义
- [ ] 为本地值守补足告警分类、确认与关键信息展示
- [ ] 打通常见故障的恢复路径与恢复后状态结论

## K11 单实例实盘策略收敛

- [ ] 为当前网格策略增加参数护栏与危险参数拦截
- [ ] 收敛更贴近实盘长期运行的风险规则和风险动作语义
- [ ] 让策略状态与最近关键变化进入查询模型、事件流和界面展示
- [ ] 让命令、成交、风险与策略状态变化形成统一复盘链路

## 当前并行方式

- 按 `K10 -> K11` 单主线串行推进
- 与多实例线程只对齐实例键、路径与事件字段，不把多实例运营能力拉进本主线
