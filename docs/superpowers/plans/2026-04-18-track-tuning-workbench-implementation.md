# Track 参数调试工作台 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个独立的 `Tauri` 桌面参数调试工作台，用于加载外部 TOML 配置、选择单个 `track` 调参、自动拉取 Binance 价格、展示关键风险指标，并支持草稿自动保存、撤销 / 重做、复制当前 `track` 与复制全部 `tracks`。

**Architecture:** 新应用放在 `tools/track-tuning-workbench/`。前端使用 `React + TypeScript` 负责交互状态、撤销 / 重做、纯试算模块、主图和高质量视觉表现；`src-tauri` Rust 侧只负责配置文件读取、受支持字段的 track 投影、Binance 合约行情适配、会话持久化、文件选择与复制能力。第一版导出不追求 TOML 原样 round-trip，只导出当前页面支持的 `track` 字段；页面未暴露字段可以省略，但页面已暴露字段需要显式导出。

**Tech Stack:** `Tauri 2`、Rust、`toml_edit`、`reqwest`、`React`、TypeScript、`Vite`、`Vitest`、`@testing-library/react`、自绘 SVG 图表。

---

## 文件结构与职责

### 根目录与 workspace

- Modify: `Cargo.toml`
  - 把 `tools/track-tuning-workbench/src-tauri` 加入 workspace，确保 Rust 侧可以直接复用 workspace edition / dependency 约定，并允许最小粒度执行 `cargo test -p poise-track-tuning-workbench ...`

### 工具目录

- Create: `tools/track-tuning-workbench/package.json`
- Create: `tools/track-tuning-workbench/pnpm-lock.yaml`
- Create: `tools/track-tuning-workbench/tsconfig.json`
- Create: `tools/track-tuning-workbench/tsconfig.node.json`
- Create: `tools/track-tuning-workbench/vite.config.ts`
- Create: `tools/track-tuning-workbench/index.html`
- Create: `tools/track-tuning-workbench/src/main.tsx`
- Create: `tools/track-tuning-workbench/src/app/App.tsx`
- Create: `tools/track-tuning-workbench/src/app/AppShell.tsx`
- Create: `tools/track-tuning-workbench/src/styles/tokens.css`
- Create: `tools/track-tuning-workbench/src/styles/base.css`
  - 前端入口、样式 token、全局视觉语言和开发脚本都放在工具目录，不污染仓库根目录

### 前端领域层

- Create: `tools/track-tuning-workbench/src/domain/trackDraft.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackValidation.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackMetrics.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackCurve.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackRisk.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackFixtures.test.ts`
  - 统一承载可编辑字段、校验、曲线、风险和派生指标，所有 UI 卡片与主图都从这一层取值

### 前端状态层

- Create: `tools/track-tuning-workbench/src/state/workbenchStore.ts`
- Create: `tools/track-tuning-workbench/src/state/history.ts`
- Create: `tools/track-tuning-workbench/src/state/sessionSync.ts`
- Create: `tools/track-tuning-workbench/src/state/workbenchStore.test.ts`
  - 承载当前文件、当前草稿、选中 `track`、撤销 / 重做、自动保存节流和命令层交互

### 前端 UI 层

- Create: `tools/track-tuning-workbench/src/ui/sidebar/FilePanel.tsx`
- Create: `tools/track-tuning-workbench/src/ui/sidebar/TrackList.tsx`
- Create: `tools/track-tuning-workbench/src/ui/metrics/MetricCards.tsx`
- Create: `tools/track-tuning-workbench/src/ui/chart/TrackWorkbenchChart.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/IdentitySection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/PriceBandSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/ExposureSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/RiskSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/CurveSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/common/InlineNotice.tsx`
- Create: `tools/track-tuning-workbench/src/ui/common/StatusBadge.tsx`
- Create: `tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx`
  - 左栏、主图、指标卡和编辑区按知识边界拆开，避免单个“巨型页面组件”

### Tauri Rust 侧

- Create: `tools/track-tuning-workbench/src-tauri/Cargo.toml`
- Create: `tools/track-tuning-workbench/src-tauri/build.rs`
- Create: `tools/track-tuning-workbench/src-tauri/tauri.conf.json`
- Create: `tools/track-tuning-workbench/src-tauri/capabilities/default.json`
- Create: `tools/track-tuning-workbench/src-tauri/src/main.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/lib.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/binance_quote.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/session_store.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/error.rs`
  - `config_document.rs` 拥有配置读取与 track 导出知识；`binance_quote.rs` 拥有 Binance 合约行情知识；`session_store.rs` 拥有草稿持久化落盘知识；`commands.rs` 只做窄接口拼装

### 文档

- Create: `tools/track-tuning-workbench/README.md`
- Modify: `docs/superpowers/specs/2026-04-18-track-tuning-workbench-design.md` 仅当实现约束逼出微调时

## 关键设计决定

### 1. 第一版导出只覆盖当前页面支持的字段

前端只编辑明确暴露的字段，例如 `track_id`、`symbol`、`lower_price`、`shape_family`、`daily_loss_limit` 等。Rust 导出层只序列化这一组字段；未暴露字段不在第一版 round-trip 范围内，但只要页面暴露了该字段，导出时就显式写出。这样可以把第一版的复杂度收在“好用的调参工作台”，不把精力耗在 TOML 形状保真上。

### 2. 前端负责试算，但必须用固定 fixture 把口径钉住

主图、指标卡和输入反馈必须在本地即时刷新，所以试算逻辑留在前端。为了避免和 `poise-core` 公式漂移，计划中会为 `linear / inertial / responsive`、纯多 / 纯空 / 对称、带外策略和风险指标建立固定 fixture，用 `Vitest` 钉住当前语义。主图采样必须直接来自这一套统一试算模块，不允许额外维护一套“只给图表看的曲线”。

### 3. Binance 价格固定走合约行情

Rust 行情层只请求 Binance 合约公共价格。前端只关心“当前价是否可用、失败原因是什么、更新时间是什么”，不承担源选择细节，也不做现货 fallback。

### 4. 图表不用通用后台图表库

主图直接用 SVG 自绘。这样可以把价格带、零仓目标点、当前价、最小步长、风险方向和曲线强弱放进同一个视觉系统里，避免落成普通后台折线图。

## Task 1: 建立 Tauri 工具骨架与基础视觉骨架

**Files:**

- Modify: `Cargo.toml`
- Create: `tools/track-tuning-workbench/package.json`
- Create: `tools/track-tuning-workbench/tsconfig.json`
- Create: `tools/track-tuning-workbench/tsconfig.node.json`
- Create: `tools/track-tuning-workbench/vite.config.ts`
- Create: `tools/track-tuning-workbench/index.html`
- Create: `tools/track-tuning-workbench/src/main.tsx`
- Create: `tools/track-tuning-workbench/src/app/App.tsx`
- Create: `tools/track-tuning-workbench/src/app/AppShell.tsx`
- Create: `tools/track-tuning-workbench/src/styles/tokens.css`
- Create: `tools/track-tuning-workbench/src/styles/base.css`
- Create: `tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx`
- Create: `tools/track-tuning-workbench/src-tauri/Cargo.toml`
- Create: `tools/track-tuning-workbench/src-tauri/build.rs`
- Create: `tools/track-tuning-workbench/src-tauri/tauri.conf.json`
- Create: `tools/track-tuning-workbench/src-tauri/capabilities/default.json`
- Create: `tools/track-tuning-workbench/src-tauri/src/main.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/lib.rs`

- [x] **Step 1: 先写前端壳层测试，钉住页面骨架区块**
  - 用 `@testing-library/react` 写一个最小渲染测试，要求壳层同时出现“文件操作区”、“Track 列表区”、“关键指标区”、“主图区”、“参数编辑区”五个 landmark 文本
  - 测试文件：`tools/track-tuning-workbench/src/ui/app/AppShell.test.tsx`

- [x] **Step 2: 初始化本地前端工程，不引入根级 Node workspace**
  - 采用 `pnpm` 管理 `tools/track-tuning-workbench` 目录内的依赖和脚本
  - `package.json` 至少定义 `dev`、`build`、`test`、`tauri` 四个 script
  - `vite.config.ts` 只服务当前工具目录，不影响仓库其他内容

- [x] **Step 3: 创建最小 Tauri 壳与 workspace 接入**
  - 把 `tools/track-tuning-workbench/src-tauri` 加入根 workspace
  - Rust crate 名称固定为 `poise-track-tuning-workbench`
  - `tauri.conf.json` 指向 `../dist` 作为 `frontendDist`，开发命令使用 `pnpm run dev`

- [x] **Step 4: 搭出第一版视觉骨架，不先堆真实数据**
  - `AppShell` 只渲染静态区块和占位说明，但要先落样式 token 和整体布局
  - `tokens.css` 先定义颜色、圆角、阴影、间距、字体层级；不要使用默认浏览器样式拼装后台页

- [x] **Step 5: 运行最小前端测试，确认壳层通过**

Run: `pnpm --dir tools/track-tuning-workbench test -- AppShell`

Expected: `PASS`，壳层测试命中 1 个文件并通过

- [x] **Step 6: 运行最小 Rust smoke test，确认 crate 可编译**

Run: `cargo test -p poise-track-tuning-workbench -- --list`

Expected: 列出空或极少量测试，但 crate 能成功参与 workspace 编译

- [x] **Step 7: Commit**

```bash
git add Cargo.toml tools/track-tuning-workbench
git commit -m "feat(workbench): scaffold tauri tuning app shell"
```

Commit SHA: `c585fed`, `0c11f64`

## Task 2: 实现 TOML 文档边界与 Track 导出模型

**Files:**

- Create: `tools/track-tuning-workbench/src-tauri/src/config_document.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/config_projection.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/lib.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/Cargo.toml`

- [x] **Step 1: 先写 Rust 单元测试，钉住当前导出边界**
  - 至少覆盖：
    - 只导出 `[[tracks]]`，不带顶层 `exchange`
    - 未暴露字段，例如 `tick_timeout_secs`，第一版不会出现在导出结果里
    - 删除一个 `track` 不影响其他 `track` 的导出顺序
    - 复制 `track` 时只复制当前页面支持的字段
    - 新建空白 `track` 时只写当前页面支持的字段
    - 页面已暴露字段即使值等于默认值也仍然显式导出

- [x] **Step 2: 在 `config_document.rs` 中实现配置读取与 `track` 投影**
  - 读取 TOML 原文并抽取 `[[tracks]]`
  - 把每个 `track` 投影成前端可编辑的 typed 字段和稳定的内部 `draft_id`
  - 不复用 `server::Config` 反序列化结构，避免 server 边界和工作台边界绑死

- [x] **Step 3: 在 `config_projection.rs` 中实现导出拼装**
  - 提供：
    - `export_current_track`
    - `export_all_tracks`
  - 导出只包含 `[[tracks]]`
  - 顺序规则明确：默认保留当前草稿顺序
  - 页面已暴露字段全部显式写出

- [x] **Step 4: 运行最小 Rust 测试**

Run: `cargo test -p poise-track-tuning-workbench config_document::tests:: -- --nocapture`

Expected: 导出范围、字段省略和复制 / 删除语义全部通过

- [x] **Step 5: Commit**

```bash
git add tools/track-tuning-workbench/src-tauri
git commit -m "feat(workbench): add track projection export boundary"
```

Commit SHA: `5a67d18`, `7b86a1b`, `fa2c966`

## Task 3: 实现 Tauri 命令层的文件、行情与会话适配

**Files:**

- Create: `tools/track-tuning-workbench/src-tauri/src/commands.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/binance_quote.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/session_store.rs`
- Create: `tools/track-tuning-workbench/src-tauri/src/error.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/src/lib.rs`
- Modify: `tools/track-tuning-workbench/src-tauri/capabilities/default.json`

- [x] **Step 1: 先写 Rust 测试，钉住命令层对外契约**
  - 至少覆盖：
    - 加载外部配置文件会返回 projected tracks
    - 行情查询固定走 Binance 合约
    - 不支持 symbol 时返回可显示错误，不 panic
    - 草稿会按配置文件路径隔离保存和恢复

- [x] **Step 2: 实现 `binance_quote.rs`**
  - 用 `reqwest` 调 Binance 合约公共接口
  - 返回结构至少包含：
    - `price`
    - `retrieved_at`
    - `error_kind`
  - 行情失败不应污染其他命令

- [x] **Step 3: 实现 `session_store.rs`**
  - 以配置文件绝对路径推导稳定 session key
  - 草稿落盘为 JSON
  - Rust 只做存取，不维护撤销历史

- [x] **Step 4: 在 `commands.rs` 中暴露窄 Tauri command**
  - 至少包括：
    - `open_config_file`
    - `load_config_file`
    - `load_saved_draft`
    - `save_draft`
    - `copy_text`
    - `fetch_binance_quote`

- [x] **Step 5: 运行最小 Rust 测试**

Run: `cargo test -p poise-track-tuning-workbench commands::tests:: -- --nocapture`

Expected: 文件加载、合约行情、会话隔离三个类别全部通过

- [x] **Step 6: Commit**

```bash
git add tools/track-tuning-workbench/src-tauri
git commit -m "feat(workbench): add tauri file quote and session commands"
```

Commit SHA: `79d42ca`, `3c20f09`, `952fd96`

## Task 4: 实现前端统一试算模块与 fixture

**Files:**

- Create: `tools/track-tuning-workbench/src/domain/trackDraft.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackValidation.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackMetrics.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackCurve.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackRisk.ts`
- Create: `tools/track-tuning-workbench/src/domain/trackFixtures.test.ts`

- [x] **Step 1: 先写 `Vitest` fixture，钉住当前公式语义**
  - fixture 至少覆盖：
    - 对称 `linear`
    - 纯空 `linear`
    - `responsive`
    - `inertial`
    - 当前价在带外
    - `desired_exposure = 0` 的零仓目标点
    - 最小步长上下非对称的情形

- [x] **Step 2: 实现统一的 typed draft 模型**
  - 把数值字段、枚举字段、附加字段和 UI 临时价格拆开
  - 允许 UI 处于“输入暂时非法但仍可继续编辑”的状态

- [x] **Step 3: 实现 `trackMetrics.ts` / `trackCurve.ts` / `trackRisk.ts`**
  - 输出至少包括：
    - 当前目标仓位
    - `1 unit` 对应价格与数量
    - 当前价到风险边缘
    - 零仓目标点到风险边缘
    - 最小步长对应价格
    - 主图所需的曲线采样点
  - `inertial / responsive` 的最小步长按数值搜索实现，不做线性近似
  - 主图曲线采样点直接来自真实公式，不单独维护图表专用近似曲线

- [x] **Step 4: 运行最小前端领域测试**

Run: `pnpm --dir tools/track-tuning-workbench test -- trackFixtures`

Expected: fixture 全部通过，数值误差在预设容忍范围内

- [x] **Step 5: Commit**

```bash
git add tools/track-tuning-workbench/src/domain
git commit -m "feat(workbench): add track tuning calculation module"
```

Commit SHA: `0096277`, `98126a2`, `405e03c`

## Task 5: 实现草稿状态、撤销 / 重做与自动保存

**Files:**

- Create: `tools/track-tuning-workbench/src/state/history.ts`
- Create: `tools/track-tuning-workbench/src/state/sessionSync.ts`
- Create: `tools/track-tuning-workbench/src/state/workbenchStore.ts`
- Create: `tools/track-tuning-workbench/src/state/workbenchStore.test.ts`
- Modify: `tools/track-tuning-workbench/src/app/App.tsx`

- [x] **Step 1: 先写状态测试，钉住用户明确要求的历史语义**
  - 至少覆盖：
    - 删除 `track` 后可撤销回来，之前可编辑字段完整保留
    - 参数输入只有在失焦 / 回车后才入栈
    - 滑杆拖动过程不重复入栈，松手后入栈一次
    - 切换 `track` 不丢本地修改
    - 刷新 / 重新打开同一路径文件可恢复草稿

- [x] **Step 2: 实现 `history.ts`**
  - 维护有上限的历史栈
  - 只记录有意义的草稿变更

- [x] **Step 3: 实现 `workbenchStore.ts`**
  - 统一管理：
    - 当前文件
    - 当前草稿
    - 当前选中 `track`
    - 新增 / 复制 / 删除
    - 撤销 / 重做
    - 临时价格覆盖

- [x] **Step 4: 实现 `sessionSync.ts`**
  - 负责把 store 的稳定快照写回 Tauri `save_draft`
  - 默认节流写盘，避免每次输入都调用命令层

- [x] **Step 5: 运行最小状态测试**

Run: `pnpm --dir tools/track-tuning-workbench test -- workbenchStore`

Expected: 撤销 / 重做、删除恢复和自动保存相关测试通过

- [x] **Step 6: Commit**

```bash
git add tools/track-tuning-workbench/src/state tools/track-tuning-workbench/src/app/App.tsx
git commit -m "feat(workbench): add draft history and autosave store"
```

Commit SHA: `da0f839`, `271512b`, `a0b3d7b`

## Task 6: 实现高质量视觉工作台与自绘主图

**Files:**

- Create: `tools/track-tuning-workbench/src/ui/sidebar/FilePanel.tsx`
- Create: `tools/track-tuning-workbench/src/ui/sidebar/TrackList.tsx`
- Create: `tools/track-tuning-workbench/src/ui/metrics/MetricCards.tsx`
- Create: `tools/track-tuning-workbench/src/ui/chart/TrackWorkbenchChart.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/IdentitySection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/PriceBandSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/ExposureSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/RiskSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/editor/sections/CurveSection.tsx`
- Create: `tools/track-tuning-workbench/src/ui/common/InlineNotice.tsx`
- Create: `tools/track-tuning-workbench/src/ui/common/StatusBadge.tsx`
- Modify: `tools/track-tuning-workbench/src/styles/tokens.css`
- Modify: `tools/track-tuning-workbench/src/styles/base.css`
- Modify: `tools/track-tuning-workbench/src/app/AppShell.tsx`

- [x] **Step 1: 先写组件测试，钉住主要交互入口**
  - 至少覆盖：
    - 左栏存在“选择配置文件 / 撤销 / 重做 / 复制当前 Track / 复制全部 Tracks”
    - 指标卡会展示“当前价格 / 最小步长对应价格 / 当前价到风险边缘 / 零仓目标点到风险边缘”
    - 删除 `track` 后有“可撤销”提示
    - 风险字段非法时有即时提示，但编辑不中断

- [x] **Step 2: 完成左栏与文件操作区**
  - 左栏显示草稿状态、当前文件路径、Track 列表和新增 / 复制 / 删除操作
  - 当前选中项、已修改项和带错误项有不同视觉状态

- [x] **Step 3: 完成顶部指标卡**
  - 指标卡要有主值、辅值和来源说明
  - Binance 价格来源和失败原因有单独 badge / note

- [x] **Step 4: 完成主图**
  - 使用 SVG 自绘：
    - 价格带
    - 曲线形状
    - 当前价格
    - 零仓目标点
    - 风险方向
    - 最小步长提示
  - 保证主图是视觉中心，不退化成小附图
  - 图上曲线必须直接消费 `trackCurve.ts` 的真实采样结果，而不是组件内再推导一套视觉曲线

- [x] **Step 5: 完成参数编辑器**
  - 四组区块：
    - 标识
    - 价格带
    - 仓位与调仓
    - 止损与带外策略
    - 曲线与预览
  - 输入提交边界与 store 约定一致

- [x] **Step 6: 运行最小组件测试**

Run: `pnpm --dir tools/track-tuning-workbench test -- AppShell`

Expected: 关键入口和风险提示相关测试通过

- [x] **Step 7: Commit**

```bash
git add tools/track-tuning-workbench/src/ui tools/track-tuning-workbench/src/styles tools/track-tuning-workbench/src/app/AppShell.tsx
git commit -m "feat(workbench): build visual tuning workspace"
```

Commit SHA: `17d9e20`, `6abe06a`, `50543ce`

## Task 7: 联通命令层、复制导出与最终验收

**Files:**

- Modify: `tools/track-tuning-workbench/src/app/App.tsx`
- Modify: `tools/track-tuning-workbench/src/state/workbenchStore.ts`
- Modify: `tools/track-tuning-workbench/src/ui/sidebar/FilePanel.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/sidebar/TrackList.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/metrics/MetricCards.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/chart/TrackWorkbenchChart.tsx`
- Modify: `tools/track-tuning-workbench/src/ui/editor/TrackEditor.tsx`
- Create: `tools/track-tuning-workbench/README.md`
- Modify: `docs/superpowers/specs/2026-04-18-track-tuning-workbench-design.md` 仅当实现与设计存在已确认偏差

- [x] **Step 1: 把前端与 Tauri commands 全部接通**
  - 文件选择、配置加载、草稿恢复、Binance 价格刷新、复制文本、保存草稿全部走真实命令层
  - UI 不再依赖假数据

- [x] **Step 2: 完成复制动作与错误态**
  - `复制当前 Track`
  - `复制全部 Tracks`
  - 文件解析失败、symbol 不支持、网络失败、字段非法时都有明确可读反馈

- [x] **Step 3: 补齐工具 README**
  - 说明如何启动开发环境
  - 说明工具边界：不回写原文件、只导出 `[[tracks]]`
  - 说明自动保存和撤销 / 重做语义

- [x] **Step 4: 跑前端最小验收**

Run: `pnpm --dir tools/track-tuning-workbench test`

Expected: 前端领域测试、状态测试和组件测试全部通过

- [x] **Step 5: 跑 Rust 最小验收**

Run: `cargo test -p poise-track-tuning-workbench`

Expected: TOML 边界、命令层和会话测试全部通过

- [x] **Step 6: 跑桌面开发 smoke**

Run: `pnpm --dir tools/track-tuning-workbench tauri dev`

Expected: 能打开桌面窗口；可手动完成一次“加载文件 -> 选择 track -> 调参 -> 删除并撤销 -> 复制当前 / 全部 tracks”的流程

- [x] **Step 7: Commit**

```bash
git add tools/track-tuning-workbench docs/superpowers/specs/2026-04-18-track-tuning-workbench-design.md
git commit -m "feat(workbench): ship track tuning desktop tool"
```

Commit SHA: `7f5194c`

## Self-Review

### Spec coverage

- 外部配置文件加载：Task 3 / Task 7
- 单 `track` 调参：Task 5 / Task 6 / Task 7
- Binance 最新价格：Task 3 / Task 7
- 草稿自动保存：Task 3 / Task 5
- 撤销 / 重做：Task 5
- 新增 / 复制 / 删除 `track`：Task 2 / Task 5 / Task 6 / Task 7
- 只复制 `[[tracks]]`、支持当前 / 全部两种复制：Task 2 / Task 7
- 当前价格、最小步长、当前价到风险边缘、零仓目标点到边缘：Task 4 / Task 6
- 高质量视觉和主图：Task 1 / Task 6
- 导出只覆盖当前页面支持字段，且这些字段全部显式写出：Task 2 / Task 7

未发现需要额外拆出的子计划；当前任务虽然跨前端、Rust 和桌面壳，但围绕同一个独立应用边界，拆成一个执行计划更利于连续验收。

### Placeholder scan

- 没有留下 `TODO`、`TBD` 或“实现细节后补”的占位文字
- 每个 task 都给出了明确文件落点、最小测试入口和 commit message

### Type consistency

- `track` typed draft、零仓目标点、最小步长、当前价到风险边缘等术语在任务间保持一致
- Rust 侧只承载 `config_document / commands / session_store / binance_quote`，前端只承载 `domain / state / ui`，没有重复命名同一职责
