# Track Tuning Workbench

`track-tuning-workbench` 是一个独立的 Tauri 调参工具。它加载外部 TOML 配置，让使用者编辑单个 `track`，并调用离线 `engine::decision_evaluation` 对比 curve-immediate benchmark 和 delayed decision replay；其中 delayed lane 复用真实 manager 决策路径。

工具不会回写原配置文件。导出、实时报价和 Poise server live inspector 与离线 evaluator 相互独立。

## 计算与展示边界

`engine::decision_evaluation` 是两条 lane 比较和评估指标的唯一所有者。它内部组合 core 的 curve-immediate benchmark 与真实 manager decision replay，负责数量、名义价值、保证金、费用、tracking error、backlog、release 和 matched price improvement。

Workbench 的 Tauri Rust adapter 只负责：

- 读取配置、投影草稿、提供 risk acquisition 默认值和生成导出 TOML。
- 构造完整、版本化的 `InstrumentSpec` 与 fee-rate fixture。
- 通过 core API 采样曲线、求 zero-target，并构造方向场景和时间路径。
- 调用单一 engine evaluator，把 report 投影为稳定的 Workbench DTO。

TypeScript 只负责：

- 表单即时校验和草稿交互。
- 异步调用 Rust evaluator，并丢弃已经过期的响应。
- 展示 instrument fixture、assumptions、两条 lane、四层 target、`DecisionReason`、revision 和 replay rows。
- 把 Rust 返回的 `curve_points` 与 replay rows 映射为 SVG 坐标。

TypeScript 不计算曲线、仓位数量、risk acquisition、费用或策略指标，也没有 evaluator fallback。

## Instrument fixture 与评估场景

每次评估都必须在界面中显式选择一个完整 instrument fixture：

- `base_asset_v1`
- `inverse_contract_v1`

fixture 由 Rust 定义并随 report 回传完整的标的语义与费率摘要。它不会写入 track 草稿、session snapshot 或导出的 TOML。fixture 是确定性评估输入，不是交易所实时 metadata。

Rust 会根据每个非零容量方向生成场景：

- `price_advantage_sweep_long` / `price_advantage_sweep_short`
- `stale_hold_long` / `stale_hold_short`

每个场景从该 Track 曲线的真实 zero-target 开始，再沿对应风险方向运行。只有 long 容量或只有 short 容量的配置只生成该方向的场景。

两条对照 lane 使用同一个 Rust 场景和同一份 instrument fixture：

- `curve_immediate_benchmark`：曲线目标变化达到 `min_rebalance_units` 后，按简化 taker 假设立即跟到目标的基准。
- `delayed_decision_replay`：经过 risk acquisition gate 的延迟决策路径。

delayed lane 第一版使用 `ImmediateFullAtOrderPrice` fill model；curve-immediate benchmark 则在目标变化达到 `min_rebalance_units` 后按样本价和简化 taker 假设立即跟到目标。两者和 instrument fixture 都是确定性评估假设，不代表真实成交概率、排队、滑点或 PnL 预测。report 会明确展示这些 assumptions。

## 草稿与临时评估状态

- 加载配置后，工作集作为本地草稿保存；重新打开同一路径时优先恢复草稿。
- 撤销和重做覆盖参数修改、新增、复制和删除 Track。
- evaluator report、instrument fixture 选择和场景选择都是可丢弃的界面状态，不进入持久化 store 或 session snapshot。
- 草稿或 fixture 变化会发起新评估；只有最新一代响应可以更新界面。
- 新请求期间清除旧 report 并显示 loading；失败后保持无 report 状态并显示错误。
- 策略配置错误会阻止 evaluator 调用；报价缺失、报价网络错误或非法手工报价只影响报价栏，不阻止 Rust 合成场景。
- 浏览器开发模式会明确显示 Rust evaluator 不可用，不运行 TypeScript 替代计算。

## 报价与 Server live

- Tauri 桌面版根据配置中的 `exchange_venue` 获取公共合约报价；当前支持 Binance 和 OKX。
- “临时覆盖价格”只影响报价栏，不进入 evaluator 请求或导出 TOML。
- 远端报价不进入撤销历史、session snapshot 或 dirty 判断。
- Server live inspector 只在 Tauri 桌面版启用，默认连接 `http://127.0.0.1:8000`。
- Server live 展示运行实例的四层 target、decision reason、revision、仓位、risk acquisition 和执行绑定；它不是离线 evaluator report 的数据源。

## 复制导出

支持两种复制动作：

- `复制当前 Track`：只复制当前选中的一个 `[[tracks]]`。
- `复制全部 Tracks`：复制当前工作集中的全部 `[[tracks]]`。

两种动作都由 Rust 命令层生成文本，再写入系统剪贴板。导出不包含 `exchange` 等顶层配置，页面未暴露的字段也不会被猜测或补写。

## 开发与验证

在仓库根目录安装依赖：

```bash
pnpm --dir tools/track-tuning-workbench install
```

启动 Tauri 桌面开发环境：

```bash
pnpm --dir tools/track-tuning-workbench tauri dev
```

只启动浏览器界面预览：

```bash
pnpm --dir tools/track-tuning-workbench dev
```

浏览器预览适合检查表单和布局，但不能运行 Rust evaluator、真实配置命令或 Server live。

最小验收：

```bash
cargo test -p poise-track-tuning-workbench
pnpm --dir tools/track-tuning-workbench test
pnpm --dir tools/track-tuning-workbench build
```

整体 decision ownership 和运行语义见 [system-overview](../../docs/system-overview.md)。
