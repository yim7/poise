---
description: 
alwaysApply: true
---

- 这是一个探索项目，目的是探索优秀的产品设计，随时可以推翻旧的
- 依赖默认是 crate 级别，项目共用的放到 workspace
- 测试先行：先定义或补齐验收测试，再实现功能；本地测试通过才算完成
- 默认先跑与改动直接相关的最小测试，不要把 `cargo test`、`cargo test --workspace` 或其他全量检查当作默认起手式
- 只有在改动跨多个 crate、需要最终验收、用户明确要求，或局部测试不足以覆盖影响面时，才扩大到 crate 级或 workspace 级测试
- 按改动的位置用这些最小入口：
  - `strategy/src/<模块>.rs`：`cargo test -p poise-strategy <模块>::`
  - `runtime/tests/<主题>.rs`（验收测试按主题分文件）：`cargo test -p poise-runtime --test <主题>`；库内单元测试：`cargo test -p poise-runtime --lib <模块>::`
  - `hyperliquid/src`：`cargo test -p poise-hyperliquid <测试名关键词>`
  - `cli/src`：`cargo test -p poise-cli`
  - `console/src`（网页）：在 `console/` 下 `npx vitest run <文件名>`，改完再 `npm run build`
- 只改脚本、README、spec、plan 等非 Rust 代码时，先做最小验证；不要默认触发全量 Rust 测试
- 探索阶段优先保持实现干净，废弃方案直接删除，不保留过渡文档和代码
- markdown 使用相对路径
- 沟通语言要直接准确，少用比喻，禁用互联网黑话，比如「收口」、「一刀」
- 验收完同步任务清单
- `plan` 仅指已确认的实现计划或任务清单，不包括讨论、分析、brainstorming、设计评审或仅写 spec
- 只有执行 `plan` 时，task 验收通过后才必须立即提交，并记录 commit SHA 回写任务清单
- 未完成 `git add`、`git commit` 和任务清单回写，不得开始下一个 plan task
- 非执行 `plan` 阶段自行判断是否提交：一件事做完、相关测试和检查都通过就提交，一个提交只做一件事；做到一半、测试没通过、还有问题等用户决定时不提交。推送仍要用户明确要求
- 如果因工作区脏状态、冲突或其他原因无法提交，必须明确报告原因，并停止推进后续 task
