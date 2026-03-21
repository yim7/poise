# TUI Locale Switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `tui` 增加 `zh-CN / en-US` 双语切换能力，支持启动时指定默认语言与运行时按 `l` 热切换，同时保持服务端返回文本原样显示。

**Architecture:** 在 `tui` 内新增统一 `locale` 模块承载本地界面文案，并将 `runtime / state / input / store / render / selectors` 接到同一套 locale 状态与文案访问入口。先通过测试定义 locale 解析、切换、翻译完整性和双语快照，再逐步替换当前散落在多个模块中的显示文本。

**Tech Stack:** Rust 2024、ratatui、crossterm、insta、cargo test

---

## 文件结构

### 新建文件

- `tui/src/locale.rs`
  - 定义 `Locale`
  - 负责环境变量解析、语言切换、文案查表、短版/长版文案
- `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_zh_cn_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_zh_cn_80x24.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_zh_cn_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_zh_cn_80x24.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__help_render_snapshot_waiting_first_snapshot_zh_cn_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_waiting_first_snapshot_zh_cn_100x16.snap`
- `tui/src/snapshots/grid_platform_tui__render__tests__market_render_snapshot_snapshot_retrying_zh_cn_100x16.snap`

### 重点修改文件

- `tui/src/lib.rs`
  - 导出 `locale` 模块
- `tui/src/runtime.rs`
  - 为 `AppConfig` 增加 `ui_locale`
  - 从 `GRID_PLATFORM_UI_LOCALE` 解析默认语言
- `tui/src/state.rs`
  - 为 `UiState` 增加 locale
  - 将 `focus_label` 改为语义标识或接收 locale
- `tui/src/events.rs`
  - 新增 `KeyAction::ToggleLocale`
- `tui/src/input/mod.rs`
  - 将 `l` 映射到切换动作
- `tui/src/store.rs`
  - 处理语言切换
  - 将本地 toast 与 bootstrap 提示改为 locale 文案
- `tui/src/selectors.rs`
  - 将本地生成的状态提示、风险建议改为 locale 文案或语义值
- `tui/src/render.rs`
  - 页签、面板标题、表头、帮助页、footer、bootstrap、modal 接入 locale
  - 新增中英文快照测试辅助
- `tui/tests/language_consistency.rs`
  - 改为翻译完整性与本地文案边界测试
- `tui/tests/local_paper_e2e.rs`
  - 增加语言切换不影响重连与命令链路的最小回归测试
- `tui/README.md`
  - 补 `GRID_PLATFORM_UI_LOCALE` 与 `l` 快捷键说明

---

### Task 1: 建立 Locale 基础与启动配置

**Files:**
- Create: `tui/src/locale.rs`
- Modify: `tui/src/lib.rs`
- Modify: `tui/src/runtime.rs`
- Modify: `tui/src/state.rs`
- Test: `tui/src/locale.rs`

- [ ] **Step 1: 写失败测试，锁定 `Locale` 解析与切换语义**

```rust
#[cfg(test)]
mod tests {
    use super::Locale;

    #[test]
    fn parses_supported_env_values() {
        assert_eq!(Locale::from_env_value("en-US"), Some(Locale::EnUs));
        assert_eq!(Locale::from_env_value("zh-CN"), Some(Locale::ZhCn));
    }

    #[test]
    fn rejects_unknown_env_values() {
        assert_eq!(Locale::from_env_value("fr-FR"), None);
    }

    #[test]
    fn toggle_switches_between_two_supported_locales() {
        assert_eq!(Locale::EnUs.toggle(), Locale::ZhCn);
        assert_eq!(Locale::ZhCn.toggle(), Locale::EnUs);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui locale::tests --lib`
Expected: FAIL，提示 `locale` 模块、`Locale` 或测试目标不存在

- [ ] **Step 3: 只实现最小 Locale 与启动配置**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    EnUs,
    ZhCn,
}

impl Locale {
    pub fn from_env_value(raw: &str) -> Option<Self> {
        match raw {
            "en-US" => Some(Self::EnUs),
            "zh-CN" => Some(Self::ZhCn),
            _ => None,
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::EnUs => Self::ZhCn,
            Self::ZhCn => Self::EnUs,
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-tui locale::tests --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/locale.rs tui/src/lib.rs tui/src/runtime.rs tui/src/state.rs
git commit -m "feat: add tui locale foundation"
```

### Task 2: 接入输入动作与状态切换

**Files:**
- Modify: `tui/src/events.rs`
- Modify: `tui/src/input/mod.rs`
- Modify: `tui/src/state.rs`
- Modify: `tui/src/store.rs`
- Test: `tui/src/input/mod.rs`
- Test: `tui/src/store.rs`

- [ ] **Step 1: 先写失败测试，锁定 `l` 映射和切换后的状态流转**

```rust
#[test]
fn plain_l_toggles_locale() {
    let action = map_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    assert_eq!(action, Some(KeyAction::ToggleLocale));
}

#[test]
fn toggle_locale_updates_ui_state_without_changing_page() {
    let mut state = AppState::sample();
    let page_before = state.ui.page;
    let focus_before = state.ui.focus_index;

    reduce(&mut state, AppEvent::Input(InputEvent::Key(KeyAction::ToggleLocale)));

    assert_eq!(state.ui.page, page_before);
    assert_eq!(state.ui.focus_index, focus_before);
    assert_eq!(state.ui.locale, Locale::ZhCn);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui input::tests store::tests --lib`
Expected: FAIL，提示 `ToggleLocale` 未定义或断言不成立

- [ ] **Step 3: 最小实现输入与归约逻辑**

```rust
pub enum KeyAction {
    // ...
    ToggleLocale,
}

match event.code {
    KeyCode::Char('l') => Some(KeyAction::ToggleLocale),
    // ...
}

KeyAction::ToggleLocale => {
    state.ui.locale = state.ui.locale.toggle();
    state.mark_dirty(DirtyFlags { ui: true, ..DirtyFlags::default() }, true);
    Vec::new()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-tui input::tests store::tests --lib`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/events.rs tui/src/input/mod.rs tui/src/state.rs tui/src/store.rs
git commit -m "feat: add tui locale toggle action"
```

### Task 3: 为 render 接入 locale，并保住英文默认行为

**Files:**
- Modify: `tui/src/locale.rs`
- Modify: `tui/src/state.rs`
- Modify: `tui/src/render.rs`
- Test: `tui/src/render.rs`
- Snapshot: `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_100x16.snap`
- Snapshot: `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_100x16.snap`
- Snapshot: `tui/src/snapshots/grid_platform_tui__render__tests__help_render_snapshot_waiting_first_snapshot_100x16.snap`

- [ ] **Step 1: 先写失败测试，锁定中英文渲染输出**

```rust
#[test]
fn dashboard_uses_english_copy_by_default() {
    let rendered = normalized_page_string(Page::Dashboard, 100, 16, |_| {});
    assert!(rendered.contains("ExchangeOrders"));
}

#[test]
fn dashboard_can_render_chinese_copy() {
    let rendered = normalized_page_string(Page::Dashboard, 100, 16, |state| {
        state.ui.locale = Locale::ZhCn;
    });
    assert!(rendered.contains("交易所挂单"));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui render::tests::dashboard_uses_english_copy_by_default render::tests::dashboard_can_render_chinese_copy --lib`
Expected: 第一条 PASS 或保持现状，第二条 FAIL

- [ ] **Step 3: 最小实现 render 接线**

```rust
let copy = locale::copy(state.ui.locale);
let titles = copy.tabs();
let exchange_orders_title = copy.dashboard().exchange_orders_title;
```

- [ ] **Step 4: 跑 render 相关测试并刷新英文快照**

Run: `cargo test -p grid-platform-tui render::tests --lib`
Expected: PASS；英文快照仅发生有意变化

- [ ] **Step 5: 提交**

```bash
git add tui/src/locale.rs tui/src/state.rs tui/src/render.rs tui/src/snapshots
git commit -m "feat: localize tui render copy"
```

### Task 4: 接入 store 与 selectors 的本地文案

**Files:**
- Modify: `tui/src/locale.rs`
- Modify: `tui/src/store.rs`
- Modify: `tui/src/selectors.rs`
- Modify: `tui/tests/language_consistency.rs`
- Test: `tui/src/store.rs`
- Test: `tui/src/selectors.rs`
- Test: `tui/tests/language_consistency.rs`

- [ ] **Step 1: 先写失败测试，锁定翻译完整性与本地文案边界**

```rust
#[test]
fn all_supported_locales_define_same_copy_keys() {
    let en = locale::copy(Locale::EnUs);
    let zh = locale::copy(Locale::ZhCn);

    assert_eq!(en.key_count(), zh.key_count());
}

#[test]
fn risk_action_hint_changes_with_locale() {
    assert_eq!(
        risk_action_hint(Locale::ZhCn, "STOP_LOSS_TRIGGERED"),
        "恢复网格前先降低风险敞口。"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui selectors::tests store::tests --lib && cargo test -p grid-platform-tui --test language_consistency`
Expected: FAIL，提示翻译接口或本地化断言未满足

- [ ] **Step 3: 最小实现 store/selectors 接线**

```rust
fn bootstrap_blocked_message(state: &AppState) -> String {
    let copy = locale::copy(state.ui.locale);
    match state.snapshot_state {
        SnapshotBootstrapState::WaitingFirstSnapshot => copy.toast().snapshot_pending.into(),
        SnapshotBootstrapState::SnapshotRetrying { .. } => copy.toast().snapshot_retrying.into(),
        SnapshotBootstrapState::Ready => String::new(),
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p grid-platform-tui selectors::tests store::tests --lib && cargo test -p grid-platform-tui --test language_consistency`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/locale.rs tui/src/store.rs tui/src/selectors.rs tui/tests/language_consistency.rs
git commit -m "feat: localize tui store and selector copy"
```

### Task 5: 为中文模式补快照与最小回归验证

**Files:**
- Modify: `tui/src/render.rs`
- Modify: `tui/tests/local_paper_e2e.rs`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_zh_cn_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_zh_cn_80x24.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_zh_cn_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__grid_render_snapshot_zh_cn_80x24.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__help_render_snapshot_waiting_first_snapshot_zh_cn_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__dashboard_render_snapshot_waiting_first_snapshot_zh_cn_100x16.snap`
- Create: `tui/src/snapshots/grid_platform_tui__render__tests__market_render_snapshot_snapshot_retrying_zh_cn_100x16.snap`

- [ ] **Step 1: 先写失败测试，锁定中文快照与链路稳定性**

```rust
#[test]
fn dashboard_render_snapshot_zh_cn_100x16() {
    assert_page_snapshot_with_locale(Page::Dashboard, Locale::ZhCn, 100, 16, "dashboard_render_snapshot_zh_cn_100x16", |_| {});
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locale_toggle_does_not_break_pause_ack_flow() -> Result<()> {
    // 启动 app 后先切到中文，再继续走 pause 命令闭环
    Ok(())
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p grid-platform-tui render::tests --lib && cargo test -p grid-platform-tui --test local_paper_e2e`
Expected: FAIL，提示中文快照缺失或新回归测试未满足

- [ ] **Step 3: 最小实现快照 helper 与中文场景**

```rust
fn assert_page_snapshot_with_locale<F>(
    page: Page,
    locale: Locale,
    width: u16,
    height: u16,
    name: &str,
    mutate: F,
) where
    F: FnOnce(&mut AppState),
{
    // 在渲染前显式设置 state.ui.locale
}
```

- [ ] **Step 4: 跑完整 TUI 回归并确认通过**

Run: `cargo test -p grid-platform-tui`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add tui/src/render.rs tui/tests/local_paper_e2e.rs tui/src/snapshots
git commit -m "test: add tui bilingual snapshots and regression coverage"
```

### Task 6: 文档与最终验收

**Files:**
- Modify: `tui/README.md`
- Modify: `docs/plan.md`
- Modify: `TODO.md`

- [ ] **Step 1: 更新使用文档**

```md
- `GRID_PLATFORM_UI_LOCALE=zh-CN|en-US`
- 运行时按 `l` 切换界面语言
```

- [ ] **Step 2: 跑全量回归**

Run: `cargo test`
Expected: PASS

- [ ] **Step 3: 复核 K8 验收标准**

Expected:
- 默认语言仍为英文
- `GRID_PLATFORM_UI_LOCALE` 生效
- `l` 可热切换
- 服务端文本未被本地改写
- 中文窄屏快照稳定

- [ ] **Step 4: 同步任务清单**

Run:

```bash
git add tui/README.md docs/plan.md TODO.md
```

Expected: `K8` 状态、任务勾选和验证记录同步完成

- [ ] **Step 5: 提交**

```bash
git add tui/README.md docs/plan.md TODO.md
git commit -m "docs: record tui locale switch completion"
```
