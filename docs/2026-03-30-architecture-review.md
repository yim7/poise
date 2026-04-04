# 架构与代码质量评审

**日期：** 2026-03-30  
**评审范围：** 全部 7 个 crate（core, engine, protocol, storage, binance, server, tui）  
**评审框架：** Software Design Philosophy（复杂度症状 + 红旗检查）  
**状态：** Claude 初审 → Codex 复核 → 双方确认

---

## Findings

### 1. [Critical] Binance 费率常量泄漏到 engine

**位置：** `engine/src/executor.rs:171`, `engine/src/executor.rs:1142`

`BINANCE_TAKER_FEE_RATE` (0.0004) 被硬编码在交易所无关的 engine 层，用于 replacement gate 计算。这与"多交易所是一等约束"冲突。

**复杂度问题：** Information leakage — 交易所知识泄漏到不应持有它的层。接入不同费率的交易所时，修改者需要在不直觉的位置找到这个常量。

**改进方向：** 将 taker fee 作为 `ExchangeRules` 的一部分由适配层提供，或作为独立的执行策略参数传入。

---

### 2. [Important] 恢复策略三处重复

**位置：** `engine/src/manager.rs:25`, `server/src/write_service.rs:23`, `server/src/runtime.rs:45`

同一枚举逻辑（`RecoverOnly` / `RecoverAndReconcile` + `allows_follow_up_reconcile()`）在三个文件中独立定义。问题不是枚举名字重复，而是"恢复语义"没有单一 owner。

**复杂度问题：** Information leakage — 任何行为语义变化需要同步三处，编译器不会提醒。

**改进方向：** 恢复同步策略需要单一 owner，其他层只调用一个明确的同步用例，而不是继续各自保留同名语义和分支逻辑。

---

### 3. [Important] executor.rs 过大且公开面过宽

**位置：** `engine/src/executor.rs`（2360 行）

该文件包含规划、状态录入、恢复、slot 操作、替换门限判定等多组能力，约 15 个 pub 函数。

**复杂度问题：** Cognitive load — 读者难以快速定位特定职责的代码。pub API 过宽使调用者难以判断应该使用哪个函数。

**改进方向：** 按职责拆分子模块：`executor::planning`、`executor::recovery`、`executor::recording`、`executor::slots`。slot 操作降为 `pub(crate)` 或私有。

---

### 4. [Important] `restore_from_snapshot()` 缺少完整性保护

**位置：** `engine/src/runtime.rs:220`

`snapshot()` 使用 struct literal 构造 `GridRuntimeSnapshot`，漏字段会编译失败。但 `restore_from_snapshot()` 通过逐行 `self.field = snapshot.field` 赋值恢复状态，遗漏一行不会产生编译错误——字段会保留旧值，导致恢复后状态不完整。

**复杂度问题：** Change amplification + unknown unknowns — 新增字段时 `restore_from_snapshot()` 的遗漏是静默的。

**改进方向：** 让 `GridRuntime` 内嵌可持久化部分为独立结构体，restore 变成整体赋值而非逐字段复制。或在 restore 结尾加 snapshot round-trip 断言（`debug_assert_eq!(self.snapshot(), *snapshot)`）。

---

### 5. [Important] GridRuntime 运行态封装过浅

**位置：** `engine/src/runtime.rs:123`

`GridRuntime` 的 14 个字段全部 `pub`。`GridManager` 是预期的唯一 owner/mutator，但 pub 字段使得任何持有 `&mut GridRuntime` 的代码都能绕过 manager 直接改状态。状态修改的约束（如 pause 时必须同时清 `desired_exposure`）只能靠人工保证。

**复杂度问题：** Overexposure — 接口暴露了不应由外部直接修改的内部状态，不变量缺乏编译期保护。

**改进方向：** 将字段可见性收为 `pub(crate)` 或私有，通过 manager 的方法暴露必要的读写操作。

---

### 6. [Important] 读模型边界贴着 engine snapshot

**位置：** `server/src/query_service.rs:14`, `server/src/projector.rs:49`

`GridReadModelSource` 直接持有 `GridRuntimeSnapshot`（engine 内部类型）。projector 深度穿透 engine 内部结构（如 `source.snapshot.executor_state.stats.max_inventory_gap_abs.0`），engine 的 `ExecutorState` 结构变化直接传导到展示层。

这也是 `poise-protocol` 枚举与 `poise-core`/`poise-engine` 枚举长期 1:1 镜像的根因——当读模型就是 engine snapshot 本身时，protocol 类型不可能独立于 engine 演化。

**复杂度问题：** Information leakage — engine 内部结构变化的影响面扩散到 server 展示层，没有中间屏障。

**改进方向：** 引入由 server 自己拥有的 read model / read source 边界，projector 基于该边界而不是 `GridRuntimeSnapshot` 工作，不预设必须引入独立 projection store 或额外持久化形态。

---

## Design Debt

### Exposure(pub f64) 封装缺失

**位置：** `core/src/types.rs:3`

`Exposure` 的 `pub f64` 内部字段在整个代码库中被大量直接访问（`.0`），newtype 没有带来封装价值。对交易系统而言，浮点精度是已知实际故障来源，日后改变表示将是全库散弹枪修改。

**当前评估：** 不是近期高风险故障点，但应作为长期改进项跟踪。要么把它做深（私有字段 + 领域方法），要么承认它只是轻量语义标签。

---

## Cleanup

### EffectService 是 pass-through

**位置：** `server/src/effect_service.rs:1`

两个方法都是直接转发到 `StateRepositoryPort`，零额外逻辑。在探索阶段偏多余，但成本也低。保留为低优先级清理项。

---

## 正面设计信号

- **端口抽象清晰：** `ExchangePort`、`MarketDataPort`、`StateRepositoryPort` 边界合理，测试中 fake 替换干净。
- **纯函数领域逻辑：** `desired_exposure()`、`band_status()`、`evaluate_risk()` 不依赖外部状态。
- **不可变状态转换模式：** executor 的 `plan()`、`record_submit_receipt()` 等接收旧状态返回新状态。
- **依赖方向正确：** core → engine → storage/binance → server，无环。
- **测试质量高：** 每个 crate 有充分单元测试，server 含并发集成测试（`BlockingPersistence`）。
- **per-grid 互斥锁：** `GridMutationGuards` 实现细粒度并发控制。
- **协议边界独立：** `poise-protocol` 自持 wire contract 类型，projector 通过 exhaustive match 转换，编译期捕获漏改。

---

## 评审过程中的分歧记录

| 项 | Claude 初审 | Codex 复核 | 最终 |
|----|------------|-----------|------|
| protocol 枚举重复 | Critical 独立 finding | 不单列，是 #6 的症状 | 不单列，写入 #6 描述 |
| "让 protocol 复用 core 枚举" | 推荐方向 | 反对，削弱协议边界 | 撤回该方向 |
| Exposure 严重度 | Important | Design debt | Design debt |
| snapshot() 遗漏编译保护 | "三处都不会提醒" | snapshot() 会编译失败 | 仅 restore 有风险 |
| EffectService | Minor finding | 不作正式 finding | Cleanup |
| GridEffect 别名 | Minor finding | 不作正式 finding | 不列入 |

---

## 整改完成状态（2026-03-30）

- **Finding 1：已完成。** `ExchangeRules` 现在同时持有 `maker_fee_rate` 和 `taker_fee_rate`；Binance 适配层填入 VIP0 默认值，replacement gate 改为 `maker + taker + buffer`，engine 内不再保留 Binance 专属费率常量。
- **Finding 2：已完成。** 恢复同步模式已收敛为 `engine/src/manager.rs` 中唯一的 `ExchangeSyncMode`，`server` 不再维护本地副本。
- **Finding 3：已完成。** `executor.rs` 已拆成 `executor/mod.rs`、`planning.rs`、`recovery.rs`、`recording.rs`、`slots.rs`；`mod.rs` 当前 1232 行，公开面也较原始版本收窄。
- **Finding 4：已完成。** `restore_from_snapshot()` 结尾已加入 round-trip `debug_assert_eq!`，并补了缺字段回归测试。
- **Finding 5：已完成。** `GridRuntime` 字段已改为 `pub(crate)`，server 侧只通过少量只读 accessor 取值。
- **Finding 6：已完成（phase 1）。** server 侧新增 `read_model.rs`，`GridReadModel::from_snapshot(...)` 成为唯一 snapshot -> read model 接触点；`projector` 不再依赖 `GridRuntimeSnapshot`。
- **Design Debt：保留跟踪。** `Exposure(pub f64)` 没并入本轮主线整改，继续作为后续独立决策项。
- **Cleanup：已完成。** `EffectService` 已删除，调用者直接依赖 `StateRepositoryPort`。

验收结果：
- `cargo test` 全量通过。
- `rg 'BINANCE_TAKER_FEE' engine/` 无结果。
- `rg 'StartupSyncMode|ExchangeStateSyncMode' server/src/write_service.rs server/src/runtime.rs` 无结果。
- `rg 'GridRuntimeSnapshot' server/src/projector.rs` 无结果。
