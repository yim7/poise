# Poise 可靠性与可解释性优化执行清单

## 状态

本文只记录当前已确认计划的执行状态，不是长期架构事实源。长期架构和运行语义以 [system-overview.md](system-overview.md) 为准。

当前状态：执行中。当前 task 是 3.1 最终验收并同步长期文档。

生命周期：

- 已完成 goal 的长期信息已吸收到 [system-overview.md](system-overview.md) 和仓库 README。
- 已吸收的 Goal 0–2、task 细节和 commit 过程记录已经删除。
- Task 3.1 提交后先回写其 commit SHA，再删除本文，不保留已完成计划作为第二套事实源。

## Task 清单

| Task | 状态 | 验收摘要 | Commit |
| --- | --- | --- | --- |
| 3.1 最终验收并同步长期文档 | in_progress | workspace、Workbench 和长期文档已通过，等待提交与生命周期清理 | — |

## Task 3.1 最终验收并同步长期文档

改动范围：

- `README.md`
- `docs/system-overview.md`
- `docs/next-goals.md`
- `tools/track-tuning-workbench/README.md`

已完成验收：

- `cargo test --workspace` 通过；3 个慢速 E2E 按设计保持 ignored。
- `cargo build -p poise-server` 通过。
- Workbench Rust 测试在 workspace 验收中通过；TypeScript 80 个测试通过，production build 通过。
- `cargo fmt --all -- --check` 与 `git diff --check` 通过。
- 长期文档明确当前 decision ownership、fresh-session、持久化、readiness、replay、effect journal、recent PNL capability 和 Workbench evaluator 边界。
- README 的默认值、公开路由、交易所启动边界和工具说明与实现一致。
- 独立事实审查与设计审查均无剩余 P0–P2。

提交前文档验证：

```bash
rg -n "policy_target|risk_approved_target|acquisition_frontier|execution_target|fresh-session|track_effects|decision replay" README.md docs/system-overview.md tools/track-tuning-workbench/README.md
! rg -n "desired_exposure|risk_release_frontier|execution_target_exposure" README.md docs/system-overview.md tools/track-tuning-workbench/README.md
git diff --check
git status --short
```

提交后动作：

1. 将本 task 标记为 `completed`，回写验收提交 SHA。
2. 提交状态回写。
3. 删除本文并提交生命周期清理。
