# TUI 中英文切换设计

本文档定义 `tui` 页面本地文案的中英文切换方案。目标是让终端界面支持 `zh-CN` 与 `en-US` 双语切换，同时保持服务端接口、协议字段和服务端返回文本不变。

## 1. 目标

- 支持 `tui` 本地界面文案在中文与英文之间切换
- 支持启动时指定默认语言
- 支持运行时快捷键热切换语言
- 保持现有服务端接口与协议不变
- 保持默认行为稳定，首版默认语言继续为英文

## 2. 范围

本次仅覆盖 `tui` 本地生成的可见文本，包括：

- 页签标题
- 面板标题
- 表格列名
- 帮助页说明
- footer 与 bootstrap 提示
- 本地 toast
- 本地状态标签
- 本地风险提示与操作建议

本次明确不做：

- 服务端返回文本的翻译或重写
- 协议字段、命令字、事件类型的语言化改造
- 第三种语言支持
- 语言偏好持久化到磁盘

## 3. 当前现状

当前 `tui` 文案分散在以下位置：

- `tui/src/render.rs`
- `tui/src/state.rs`
- `tui/src/store.rs`
- `tui/src/selectors.rs`

当前问题有两类：

1. 文案没有统一入口，新增或修改时容易漏改。
2. 现有测试中包含“运行时代码不允许中文”的约束，不适合双语目标。

另外，`tui` 使用固定列宽和窄屏布局。中文为全角字符，英文为半角字符，同一语义切换语言后会影响：

- 换行位置
- 表头截断
- footer 宽度
- 帮助页和 bootstrap 页面排版

因此这次设计必须把“语言切换”和“布局稳定性”一起处理。

## 4. 设计原则

### 4.1 单一文案入口

所有 `tui` 本地界面文案统一收敛到独立模块，不再分散写死在渲染、状态和归约逻辑里。

### 4.2 服务端文本原样显示

服务端返回的 `ack.message`、`system event.message`、风险事件消息等，继续原样显示，不做本地翻译。

### 4.3 默认行为不回退

首版默认语言保持英文，避免当前默认行为和现有英文快照基线出现无意变化。

### 4.4 运行时可切换

语言不是只读配置，需要支持用户在当前进程内热切换，并立即重绘页面。

### 4.5 文案与视口共同建模

同一文案允许存在长版与短版，尤其是：

- 状态栏
- footer
- bootstrap 提示

不能假设一份文案同时适配所有宽度。

## 5. 模块设计

### 5.1 新增 `tui/src/locale.rs`

新增独立本地化模块，负责：

- 定义 `Locale`
- 解析与格式化语言配置
- 暴露所有本地界面文案
- 提供少量需要参数插值的文案函数

建议定义：

```rust
pub enum Locale {
    EnUs,
    ZhCn,
}
```

建议提供：

- `Locale::from_env_value(&str) -> Option<Self>`
- `Locale::toggle(self) -> Self`
- `Locale::code(self) -> &'static str`

### 5.2 文案组织方式

不建议把全部文案做成单层 `const` 字符串集合。更合适的是按页面与用途分组，例如：

- `TabsCopy`
- `DashboardCopy`
- `GridCopy`
- `HelpCopy`
- `FooterCopy`
- `ToastCopy`
- `StatusCopy`

对于带参数的文案，使用函数生成，例如：

- snapshot retry
- bootstrap error
- reconnect detail
- timeout 提示

### 5.3 视口相关文案

针对窄屏与常规宽度，提供单独接口，例如：

- `waiting_first_snapshot_short()`
- `waiting_first_snapshot_long()`
- `footer_bootstrap_waiting_short()`
- `footer_bootstrap_waiting_long()`

避免在 `render.rs` 中硬编码多语言分支。

## 6. 状态与配置设计

### 6.1 启动配置

在 `tui/src/runtime.rs` 的 `AppConfig` 中增加：

```rust
pub ui_locale: Locale
```

读取方式：

- 环境变量 `GRID_PLATFORM_UI_LOCALE`
- 允许值：`en-US`、`zh-CN`
- 未设置时默认 `en-US`

### 6.2 运行时状态

在 `tui/src/state.rs` 中为 UI 或 App 状态增加当前语言字段，建议放入 `UiState`：

```rust
pub locale: Locale
```

原因：

- 语言切换属于纯 UI 状态
- 不需要进入协议模型
- 不影响运行态、风控态和执行态

### 6.3 初始化

`run_app` 或 `run_loop` 创建初始状态后，将 `AppConfig.ui_locale` 写入状态。`AppState::waiting_first_snapshot()` 与 `AppState::sample()` 需要支持显式设置 locale，避免测试依赖隐式默认值。

## 7. 事件与交互设计

### 7.1 新增按键动作

在 `tui/src/events.rs` 中新增：

```rust
KeyAction::ToggleLocale
```

### 7.2 默认快捷键

在 `tui/src/input/mod.rs` 中将 `l` 映射为语言切换动作。

首版约定：

- `l`：在 `en-US` 与 `zh-CN` 间切换

不额外引入组合键，保持实现和帮助页说明简单。

### 7.3 归约逻辑

在 `tui/src/store.rs` 中处理 `ToggleLocale`：

- `state.ui.locale = state.ui.locale.toggle()`
- 标记 `ui` dirty
- 立即触发重绘
- 不改变当前 page、focus、modal、toast 生命周期

建议补一个本地 toast：

- 切到中文：`已切换到中文`
- 切到英文：`Switched to English`

该 toast 也属于本地文案，需由 `locale` 模块统一提供。

## 8. 渲染改造策略

### 8.1 `render.rs`

`render.rs` 不再直接持有用户可见文本常量，而是通过当前 locale 取文案。主要覆盖：

- 页签
- 面板标题
- 表头
- bootstrap 页面
- 帮助页
- footer
- 模态框说明
- 本地空态提示

### 8.2 `state.rs`

`Page::focus_label()` 当前直接返回英文标签。改造后应改为：

- 返回稳定的语义标识，或
- 接收 `Locale` 参数

推荐做法是返回语义标识，再由 `locale` 模块映射到实际文案。这样状态层不再依赖具体显示语言。

### 8.3 `store.rs`

`store.rs` 中的本地 toast 和 bootstrap 阻断提示需要改为通过 locale 获取。

### 8.4 `selectors.rs`

`selectors.rs` 当前包含本地生成的提示文本、状态标签和风险动作建议，这些也需要纳入本地化，但服务端返回的消息正文不动。

建议处理原则：

- ViewModel 尽量保留语义值，不提前绑定最终显示语言
- 只有必须在 selector 层拼出来的本地提示，才通过 locale 生成

## 9. 测试设计

### 9.1 替换现有语言一致性测试

当前 `tui/tests/language_consistency.rs` 的思路是禁止运行时出现中文。双语方案下应改为两类测试：

1. 翻译完整性测试
2. 本地文案边界测试

翻译完整性测试要求：

- `en-US` 与 `zh-CN` 覆盖同一组文案键
- 带参数文案在两种语言下都可生成

本地文案边界测试要求：

- `render.rs`、`store.rs`、`state.rs` 不再新增散落的硬编码显示文案
- 本地文案统一从 `locale` 模块取

### 9.2 输入与状态测试

需要新增单元测试覆盖：

- `l` 能映射到 `ToggleLocale`
- 切换动作能改变 `state.ui.locale`
- 切换后保留当前 page、focus、modal
- 切换后触发 UI 重绘

### 9.3 渲染快照测试

快照策略建议分两层：

- 继续保留英文快照，确保默认行为不回退
- 新增中文快照，验证中文布局稳定性

首版不必把全部快照翻倍，优先覆盖最容易受字符宽度影响的页面：

- `Dashboard`
- `Grid`
- `Help`
- waiting first snapshot
- snapshot retrying

宽度优先级：

- `100x16`
- `80x24`

若中文在 `120x18` 下也出现明显布局风险，再补该档快照。

### 9.4 回归范围

现有本地 E2E 只需要验证：

- 默认 locale 不影响当前行为
- 切换 locale 不影响命令链路和重连链路

不需要在 E2E 中穷举所有双语文案。

## 10. 验收标准

本功能完成后，应满足：

- 默认语言保持英文
- 支持 `GRID_PLATFORM_UI_LOCALE` 指定启动语言
- 支持运行时按 `l` 在中英文之间切换
- 切换后立即重绘，不影响当前页面和运行状态
- 服务端返回文本继续原样显示
- 中文模式下关键页面在窄屏布局下不出现明显错位或关键信息丢失
- `cargo test -p grid-platform-tui` 通过
- `cargo test` 通过

## 11. 实施顺序建议

建议按以下顺序落地：

1. 新增 `locale.rs`，定义 `Locale` 与文案访问接口
2. 为 `AppConfig`、`UiState`、输入事件与归约逻辑加入 locale 支持
3. 改造 `render.rs` 的页签、面板标题、bootstrap、help、footer
4. 改造 `store.rs` 和 `selectors.rs` 中的本地文本
5. 重构语言一致性测试
6. 增补双语快照
7. 跑 `cargo test -p grid-platform-tui`
8. 跑 `cargo test`

## 12. 风险与控制

### 12.1 中文布局抖动

风险最高。控制方式：

- 对关键窄屏场景补中文快照
- 为状态栏、footer、bootstrap 提供短版文案

### 12.2 文案再次散落

控制方式：

- 统一通过 `locale` 模块取文案
- 用测试约束新增本地文案入口

### 12.3 ViewModel 与显示层耦合过深

控制方式：

- 尽量保留语义值
- 最终显示文本延后到 locale 层或 render 层

## 13. 结论

本方案采用统一 `locale` 模块承载 `tui` 本地界面文案，并通过启动配置与运行时快捷键同时支持中英文切换。该方案不改服务端接口，不翻译服务端返回文本，能够在当前代码结构下以较低风险实现双语切换，同时为后续继续维护中文与英文快照提供清晰边界。
