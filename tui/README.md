# 网格平台终端客户端

这是网格平台的 Rust 终端客户端子项目。

## 当前已具备

- 基于 Ratatui 的渲染层
- `Dashboard`、`Grid`、`Market`、`Events`、`Help` 五个页面的运维版首轮实现
- 单写者 reducer / store 与 effect 驱动的 I/O 结构
- HTTP 启动快照拉取与 WebSocket 事件接收骨架
- 真实面板焦点高亮、底栏/帮助页键盘导航提示
- `pending / accepted / ack / failed / timed_out` 命令时间线
- `stale / degraded / reconnecting` 连接退化提示与危险操作风险文案
- `120x18`、`100x16`、`80x24` 窄屏快照基线与退化态快照回归
- 渲染快照测试与协议测试

## 运行

```bash
cargo run -p grid-platform-tui
```

也可以通过环境变量指定启动语言：

```bash
GRID_PLATFORM_UI_LOCALE=zh-CN cargo run -p grid-platform-tui
```

这些变量也可以写到当前工作目录或其父目录中的 `.env`。`tui` 启动时会自动尝试加载 `.env`，同名进程环境变量优先。

可选值：

- `en-US`
- `zh-CN`

运行中可按 `l` 在中英文界面之间切换。

## 测试

```bash
cargo test
```
