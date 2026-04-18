# Track Tuning Workbench

`track-tuning-workbench` 是一个独立的 Tauri 调参工具，用来加载外部 TOML 配置、选择单个 `track`、实时查看 Binance 合约价格，并在不回写原文件的前提下调试参数。

## 开发启动

安装依赖：

```bash
pnpm --dir tools/track-tuning-workbench install
```

启动桌面开发环境：

```bash
pnpm --dir tools/track-tuning-workbench tauri dev
```

只跑前端界面开发：

```bash
pnpm --dir tools/track-tuning-workbench dev
```

## 工具边界

- 只加载外部配置文件，不回写原文件。
- 导出时只复制 `[[tracks]]` 段，不包含 `exchange` 等顶层配置。
- 页面上支持的字段会全部显式写出；页面未暴露的字段不会导出。
- 当前价格默认来自 Binance 合约公共报价。
- “临时覆盖价格”只影响本地试算，不进入导出 TOML。

## 草稿、撤销与重做

- 加载配置文件后，当前工作集会作为本地草稿保存。
- 切换 `track`、刷新页面、重新打开同一个配置文件时，会优先恢复本地草稿。
- 撤销和重做覆盖：
  - 参数修改
  - 新增 `track`
  - 复制 `track`
  - 删除 `track`
- 远端 Binance 报价不会进入撤销历史，也不会影响 dirty 判断。

## 复制导出

支持两种复制动作：

- `复制当前 Track`：只复制当前选中的一个 `[[tracks]]`
- `复制全部 Tracks`：复制当前工作集中的全部 `[[tracks]]`

两种复制都通过 Rust 命令层生成导出文本，再写入系统剪贴板。
