# TUI Activity 本地时区显示设计

**日期：** 2026-03-28

**目标：** 保持服务端和协议里的 activity 时间戳继续使用 UTC RFC3339 字符串，只在 `grid-tui` 渲染 `Activity` 列表时把时间转换为运行 `grid-tui` 的机器本地时区显示。

## 背景

当前服务端 projector 会把 activity 事件时间序列化成 UTC RFC3339 字符串，`grid-tui` 直接原样显示。这样虽然协议稳定，但终端值守时阅读成本偏高，用户需要手动做时区换算。

## 决策

采用“只改 TUI 展示层”的最小方案：

- `server` 不改
- `protocol` 不改
- `tui` 解析 `activity.ts`
- 成功解析时转成本地时区格式化显示
- 解析失败时保留原字符串

## 范围

本次只改 `Activity` 列表，不改：

- `Overview` 面板里的 `updated at`
- HTTP / WebSocket 返回结构
- SQLite 中保存的时间字段

## 架构落点

- 在 `tui/src/views/instance.rs` 增加一个小的时间格式化辅助函数
- 渲染 activity 行时，通过该辅助函数把 RFC3339 UTC 字符串转换为本地时区字符串
- `tui/Cargo.toml` 增加 `chrono` 依赖以便做时区转换

## 测试策略

至少覆盖：

1. 合法 RFC3339 UTC activity 时间戳在渲染时不再原样显示 `Z` 结尾原串
2. 非法时间戳会回退为原字符串，避免渲染失败
