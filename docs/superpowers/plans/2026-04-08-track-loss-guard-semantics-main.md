# Track Loss Guard Semantics on Main

> 基于 `main` 的重构边界重新实现已确认的止损语义，执行时按 task 验收、提交，并回写 commit SHA。

相关设计：
- `docs/superpowers/specs/2026-04-08-track-loss-guard-semantics-design.md`

## Tasks

- [x] Task 1: 迁移 `core` / `engine` 风控语义与 UTC 日内净值口径
  - 验收：`cargo test -p poise-core`
  - 验收：`cargo test -p poise-engine ledger::tests`
  - 验收：`cargo test -p poise-engine reconciler::tests`
  - 验收：`cargo test -p poise-engine runtime::tests`
  - 验收：`cargo test -p poise-engine manager::tests`
  - commit: `e7530e4`

- [ ] Task 2: 迁移 `snapshot` / `storage` 兼容与 `RiskState` 瘦身
  - 验收：`cargo test -p poise-engine runtime::tests`
  - 验收：`cargo test -p poise-storage`
  - commit:

- [ ] Task 3: 迁移 `application` / `server` 的 budget read model、协议与配置边界
  - 验收：`cargo test -p poise-application`
  - 验收：`cargo test -p poise-server`
  - commit:

- [ ] Task 4: 更新 README、示例配置并执行全量回归
  - 验收：`cargo fmt --all --check`
  - 验收：`cargo test --workspace`
  - commit:
