# TUI 值守体验优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 完成 TUI 剩余的值守体验优化，让失败、重连、实例切换、危险操作和首屏等待态都能更快扫读、更少误判。

**Architecture:** 保持 `service` 协议不变，集中调整 `tui` 的 selector、store、render 和 locale。优先把“关键信息是否看得见”做实，再补帮助文案和空态说明；每一项都先写渲染或 store 失败测试，再做最小实现并更新快照基线。

**Tech Stack:** Rust、ratatui、现有 TUI reducer/store、render snapshot 测试、`cargo test`

---

## File Map

- `tui/src/render.rs`
  当前几乎所有页面渲染都在这里，包含状态栏、页脚、危险操作弹窗、`Dashboard / Grid / Market / Events / Help` 页面和 bootstrap 渲染。
- `tui/src/selectors.rs`
  负责把状态转成 UI 视图模型，适合补“相对时间”“实例索引”“命令失败摘要”等派生字段。
- `tui/src/store.rs`
  负责本地 UI 事件和 effect 结果，适合补“实例切换提示”“切换完成提示”“toast 生命周期”。
- `tui/src/state.rs`
  适合补 modal/toast 所需的最小状态扩展，前提是确实不能通过 selector 即时计算解决。
- `tui/src/locale.rs`
  新增帮助文案、危险操作上下文标签、实例切换提示、相对时间文本、空态说明。
- `tui/src/snapshots/*.snap`
  现有渲染快照基线，所有布局类改动都要更新对应快照。
- `tui/tests/local_paper_e2e.rs`
  用于验证实例切换、首屏 bootstrap、命令确认和连接恢复链路。
- `TODO.md`
  完成验收后同步任务状态与最近验证记录。

## Scope Notes

- 本计划覆盖此前分析出的“还没实现”的 UI 优化点。
- `Grid` 页“买卖双栏 + 距离百分比 + 右侧双列摘要”已经完成，不再重复规划。
- 当前工作区里有一处与本任务无关的已有改动：`service/tests/mainnet_bootstrap.rs`。执行时不要回退它。

### Task 1: 先锁定剩余 UX 缺口的失败测试

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/store.rs`
- Modify: `tui/tests/local_paper_e2e.rs`

- [ ] **Step 1: 为紧凑命令时间线补失败测试**

在 `tui/src/render.rs` 新增渲染断言，例如：

```rust
#[test]
fn compact_command_timeline_shows_latest_failure_reason() {
    let rendered = normalized_page_string(Page::Dashboard, 100, 16, |state| {
        // 构造 FAILED / TIMED OUT 命令，并给出 summary
    });

    assert!(rendered.contains("FAILED"));
    assert!(rendered.contains("upstreamtimeout"));
}
```

- [ ] **Step 2: 运行单测确认失败**

Run: `cargo test -p grid-platform-tui compact_command_timeline_shows_latest_failure_reason -- --nocapture`  
Expected: FAIL，说明紧凑布局下还看不到失败摘要

- [ ] **Step 3: 为危险操作弹窗上下文补失败测试**

在 `tui/src/render.rs` 新增渲染断言，验证确认弹窗包含：

- 当前实例 `symbol`
- 当前环境 `env`
- 当前仓位或仓位方向
- 当前健康状态
- 待处理命令数

- [ ] **Step 4: 运行单测确认失败**

Run: `cargo test -p grid-platform-tui danger_modal_includes_runtime_context -- --nocapture`  
Expected: FAIL，说明弹窗仍只有通用文案

- [ ] **Step 5: 为实例切换反馈补 store 测试**

在 `tui/src/store.rs` 新增测试，覆盖：

- 按 `]` 切换实例时立即出现“正在切换到 X”的提示
- 新实例快照加载完成后提示更新成“已切换到 X”

- [ ] **Step 6: 运行单测确认失败**

Run: `cargo test -p grid-platform-tui instance_switch_shows_transition_toasts -- --nocapture`  
Expected: FAIL，说明当前没有切换反馈

### Task 2: 让紧凑布局也能看清命令失败

**Files:**
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 扩展命令时间线视图模型**

补充一个“紧凑模式摘要”字段，优先规则：

- `FAILED` / `TIMED OUT` 显示失败摘要
- `ACCEPTED` / `PENDING` 显示等待状态
- `ACK` 维持简短成功信息

建议最小接口：

```rust
pub struct CommandTimelineItemViewModel {
    pub stage_label: &'static str,
    pub command_label: &'static str,
    pub compact_detail: Option<String>,
}
```

- [ ] **Step 2: 在紧凑布局渲染失败摘要**

调整 `command_timeline_items` 的 `compact` 分支，不再只渲染一行 badge；对失败项渲染两行：

- 第一行：阶段 + 命令名
- 第二行：失败摘要或超时原因

- [ ] **Step 3: 补本地化文案**

在 `tui/src/locale.rs` 增加紧凑失败摘要需要的前缀文案，例如：

- `错误`
- `超时`
- `等待服务端确认`

- [ ] **Step 4: 运行相关测试确认通过**

Run: `cargo test -p grid-platform-tui compact_command_timeline -- --nocapture`  
Expected: 新增测试通过，相关旧测试不回归

### Task 3: 给危险操作弹窗补足实例上下文

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 定义危险操作上下文渲染输入**

不要先扩状态；优先通过 selector 或 render 时从 `AppState` 即时计算：

- `symbol`
- `env`
- `position_qty`
- `connection health`
- `pending_commands`

- [ ] **Step 2: 调整 `draw_modal` 接口**

把 `draw_modal` 从只接收 `modal + locale` 改成能读当前 `AppState`，避免 modal 内重复存储运行态快照。

- [ ] **Step 3: 改写确认弹窗正文**

建议正文顺序：

1. 操作描述
2. 风险提示
3. 当前实例上下文
4. `Enter / Esc` 提示

示例布局：

```text
实例 BTCUSDT · testnet
仓位 0.250 · 健康 RECONNECTING · 待处理 1
```

- [ ] **Step 4: 运行相关测试确认通过**

Run: `cargo test -p grid-platform-tui danger_modal_includes_runtime_context -- --nocapture`  
Expected: 新增测试通过，现有 modal 测试不回归

### Task 4: 提高实例切换的可见性

**Files:**
- Modify: `tui/src/store.rs`
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`
- Modify: `tui/tests/local_paper_e2e.rs`

- [ ] **Step 1: 为实例切换增加过渡 toast**

在 `LocalUiEvent::SelectInstance` 流程里，切换开始时设置 `Info` toast，文案类似：

- `切换到 ETHUSDT...`
- `已切换到 ETHUSDT`

- [ ] **Step 2: 为实例列表增加索引信息**

在 `selectors::instances` 或 render 辅助函数里补：

- 当前实例序号
- 总实例数

页面中至少显示 `2/5` 这类信息。

- [ ] **Step 3: 在状态栏或 `Market > Runtime` 中显示索引**

优先放在 `Market > Runtime`，如果空间允许，再补到顶栏。

- [ ] **Step 4: 为实例切换完成补 E2E 验证**

在 `tui/tests/local_paper_e2e.rs` 增加断言：

- 切换时出现过渡提示
- 新快照 ready 后提示变为完成态

- [ ] **Step 5: 运行相关测试确认通过**

Run: `cargo test -p grid-platform-tui instance_switch -- --nocapture`  
Expected: store 和 E2E 测试通过

### Task 5: 让 `Market` 页连接状态更易扫读

**Files:**
- Modify: `tui/src/selectors.rs`
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 为连接状态补更易读的视图模型**

新增派生字段：

- 相对心跳时间，例如 `12s ago` / `12 秒前`
- 统一连接 badge 标签
- 更短的健康提示文本

- [ ] **Step 2: 把 `UP / DOWN` 文本改成 badge 风格**

优先复用现有 `badge_span` 设计，不重复造样式分支。

- [ ] **Step 3: 缩短 `Connectivity` 区说明**

健康提示从长句改成：

- 一行模式 badge
- 一行关键提示

避免把内容挤掉。

- [ ] **Step 4: 运行渲染测试确认通过**

Run: `cargo test -p grid-platform-tui market_render_snapshot -- --nocapture`  
Expected: `Market` 页快照更新后通过

### Task 6: 收敛首屏等待态和失败态

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`
- Modify: `tui/tests/local_paper_e2e.rs`

- [ ] **Step 1: 先写失败测试，锁定页面级 bootstrap 目标**

新增测试验证等待态和失败态时：

- 页面中央出现统一说明
- 各 panel 不再重复同一段 bootstrap 文案

- [ ] **Step 2: 运行单测确认失败**

Run: `cargo test -p grid-platform-tui waiting_first_snapshot_hides_repeated_panel_copy -- --nocapture`  
Expected: FAIL，说明当前还是面板级重复文案

- [ ] **Step 3: 实现页面级 bootstrap 容器**

对 `Dashboard / Grid / Market / Events` 统一改成：

- 上方保留状态栏
- 主区显示一个页面级说明块
- 各页 panel skeleton 保持简化

- [ ] **Step 4: 运行相关测试确认通过**

Run: `cargo test -p grid-platform-tui waiting_first_snapshot -- --nocapture`  
Expected: 等待态、重试态和 E2E 相关测试通过

### Task 7: 完善 Grid 空态与 Help 术语

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/locale.rs`

- [ ] **Step 1: 为 Grid 空态补失败测试**

覆盖“没有实时挂单时，左右两栏不只是空框，而是给出原因和下一步关注点”。

- [ ] **Step 2: 在空态中显示原因**

优先使用：

- 实时订单源不可用
- 当前无真实策略挂单
- 若有 `status_reason`，显示为下一步判断依据

- [ ] **Step 3: 扩充 Help 术语**

新增至少这些说明：

- `HEALTHY / DEGRADED / RECONNECTING`
- `PENDING / ACCEPTED / ACK / FAILED / TIMED OUT`
- 实例标记 `>` / `*`

- [ ] **Step 4: 运行帮助页与 Grid 渲染测试确认通过**

Run: `cargo test -p grid-platform-tui 'help_page|grid_render_snapshot' -- --nocapture`  
Expected: Help 和 Grid 相关渲染测试通过

### Task 8: 让 toast 不再遮住快捷键

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/src/state.rs`
- Modify: `tui/src/store.rs`

- [ ] **Step 1: 先写失败测试**

覆盖“有 toast 时，底栏仍能看到最核心快捷键提示”。

- [ ] **Step 2: 运行单测确认失败**

Run: `cargo test -p grid-platform-tui footer_keeps_shortcuts_visible_when_toast_is_present -- --nocapture`  
Expected: FAIL，说明 toast 仍完全覆盖底栏

- [ ] **Step 3: 实现双层底栏**

推荐做法：

- 第一层：toast 或系统提示
- 第二层：固定快捷键

若高度不足，则固定保留最小快捷键版本。

- [ ] **Step 4: 运行相关测试确认通过**

Run: `cargo test -p grid-platform-tui footer_keeps_shortcuts_visible_when_toast_is_present -- --nocapture`  
Expected: PASS

### Task 9: 全量验证、文档和任务清单

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: 运行格式化**

Run: `cargo fmt`

- [ ] **Step 2: 构建服务端二进制，保证 E2E 可用**

Run: `cargo build -p grid-platform-service`

- [ ] **Step 3: 运行 TUI 全量测试**

Run: `cargo test -p grid-platform-tui -- --nocapture`

- [ ] **Step 4: 运行服务端关键回归**

Run: `cargo test -p grid-platform-service --test binance_integration -- --nocapture`

- [ ] **Step 5: 更新 `TODO.md`**

同步：

- 本次 UI 优化对应的任务勾选状态
- 最近一次验证命令

- [ ] **Step 6: 提交**

```bash
git add TODO.md tui/src/state.rs tui/src/store.rs tui/src/selectors.rs tui/src/render.rs tui/src/locale.rs tui/src/snapshots docs/superpowers/plans/2026-03-23-tui-ux-hardening.md
git commit -m "优化 TUI 值守体验"
```
