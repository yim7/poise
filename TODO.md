# 任务追踪

- [x] 收紧 runtime 启动 bootstrap 边界
  - 设计：[`docs/superpowers/specs/2026-04-18-runtime-startup-bootstrap-boundary-design.md`](docs/superpowers/specs/2026-04-18-runtime-startup-bootstrap-boundary-design.md)
  - 计划：[`docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md`](docs/superpowers/plans/2026-04-18-runtime-startup-bootstrap-boundary.md)
  - 目标：把启动期实时交易所状态探测、保证金预检、guard seed 和初始 exchange state sync 收到同一条 runtime bootstrap 路径里，避免 `assembly` 和 `startup_sync` 各自维护一套启动探测语义。

- [ ] 移除旧价格迁移兼容逻辑
  - 背景：当前为了兼容历史 `track_snapshots` 数据，保留了 `reference_price` 旧 schema 迁移，以及缺少 `price_execution_block_reason` 的旧快照恢复兜底。
  - 目标：确认历史数据库和快照都已完成升级后，删除 `storage/src/schema.rs` 中仅用于旧 `track_snapshots` 的迁移分支，以及 `engine/src/runtime.rs` / `engine/src/price_gate.rs` 中仅用于旧快照恢复的 gate 兼容逻辑。
  - 说明：这个任务不急，后续有空再做。
