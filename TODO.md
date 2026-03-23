# TODO

当前 TODO 只跟踪最近 1 到 3 个里程碑的可执行任务。
阶段目标与顺序见 [`docs/roadmap.md`](docs/roadmap.md)，近期策略与验收标准见 [`docs/plan.md`](docs/plan.md)。

## 当前状态

- 当前主线：`K10` 单实例 `7x24` 值守硬化
- 并行线程：多实例固定区间网格已实现并并入主线，后续只维护实例边界与运行安全配套
- 最近完成：`K10` TUI 值守体验优化已实现并验收通过；`K9` 主网运行安全底座已实现并验收通过；多实例固定区间网格已实现并验收通过；`K8` TUI 中英文切换；`K7` Web UI 查询模型与能力边界预备；`K6` replay / paper / testnet 验证；`K5` 网格策略与风控；`K4` 执行闭环与命令语义做实
- 最近验证：`2026-03-23` 已通过 `cargo fmt`、`cargo build -p grid-platform-service`、`cargo test -p grid-platform-tui -- --nocapture` 与 `cargo test -p grid-platform-service --test binance_integration -- --nocapture`

## K9 主网运行安全底座

### K9.1 测试先补齐

- [x] 为 `paper / testnet / mainnet` 运行级别解析与显式开启规则补测试
- [x] 为启动前交易所快照收集补测试
- [x] 为启动对账补测试，覆盖“继续运行 / 暂停待确认”语义
- [x] 为稳定原因码持久化与查询暴露补测试
- [x] 为硬保护命中后的自动暂停补测试

### K9.2 运行级别与准入保护

- [x] 明确 `paper / testnet / mainnet` 运行级别模型
- [x] 增加 mainnet 显式开启入口，默认不能误入
- [x] 建立实例级默认 SQLite 路径推导

### K9.3 启动对账与自动暂停

- [x] 建立 Binance 启动前签名持仓 / 挂单快照收集
- [x] 建立启动对账流程
- [x] 让 mainnet 缺少签名状态时拒绝启动
- [x] 让持仓或交易所挂单不一致默认进入暂停态
- [x] 为启动暂停和运行期保护写入稳定原因码
- [x] 让 breaker 命中和连续策略同步失败自动暂停策略

### K9.4 事件、日志与实例边界

- [x] 为启动暂停和运行期保护补 `SystemEvent.code`
- [x] 为 `SystemEvent.code` 补 SQLite 持久化
- [x] 为 `/query/alerts` 暴露系统事件原因码
- [x] 为默认数据路径保留实例边界

### K9.5 收尾与验收

- [x] 跑通 K9 对应测试矩阵
- [x] 复核 K9 验收标准
- [x] 更新 `docs/plan.md` 与本文件状态
- [x] 记录最近一次验证结果
- [x] 验收结论：K9 已验收通过

## K10 单实例 `7x24` 值守硬化

- [x] 为 Binance 签名请求补 server time 校准与 `-1021` 自动恢复
- [x] 让 Binance 模式下策略挂单与撤单走真实交易所 execution 路径
- [x] 让 Binance 策略挂单按交易所过滤器归整价格和数量，并在 Hedge Mode 账号上给出明确暂停原因
- [x] 放宽批量策略补单的同步预算，避免 testnet 大网格在首单成功后因总超时过早中断
- [x] 让真实立即成交的订单先同步本地运行态，避免同一网格在下一轮策略补单中被重复提交
- [x] 完成 TUI 值守体验优化，补命令失败摘要、危险操作上下文、实例切换反馈、等待态 overlay 和连接状态扫读
- [ ] 固化单实例部署、工作目录、数据目录与日志目录约定
- [ ] 增加重复启动保护与优雅停机流程
- [ ] 抽出统一健康状态与自动降级语义
- [x] 为本地值守补足告警分类、确认与关键信息展示
- [ ] 打通常见故障的恢复路径与恢复后状态结论

## K11 单实例实盘策略收敛

- [x] 让 Grid 页策略订单只显示当前真实挂单，库存占用改由仓位和摘要表达
- [x] 调整 Grid 页策略订单为买卖双栏展示，并显示到成交比例距离
- [ ] 为当前网格策略增加参数护栏与危险参数拦截
- [ ] 收敛更贴近实盘长期运行的风险规则和风险动作语义
- [x] 为 Binance 用户流补统一执行事件翻译，覆盖异步挂单成交与部分成交
- [x] 让异步 `buy / sell fill` 一致写入 `recent_fills`、查询接口与 SQLite 审计链路
- [ ] 为用户流驱动的成交、挂单、仓位更新补去重与顺序处理，避免读模型脱节
- [ ] 让策略状态与最近关键变化进入查询模型、事件流和界面展示
- [ ] 让命令、成交、风险与策略状态变化形成统一复盘链路

## 并行线程：多实例固定区间网格（已验收）

### K9.1 服务端配置与注册表

- [x] 增加环境级 TOML 配置文件模型与校验
- [x] 支持 `service --config <path>` 启动多标的实例
- [x] 自动推导 `.data/<environment>/<symbol-lowercase>.db` 持久化路径
- [x] 增加多实例注册表与默认 symbol 兼容别名

### K9.2 固定区间梯子策略

- [x] 协议改为 `lower_price / upper_price / grid_levels / max_position_notional`
- [x] 用固定区间梯子替代中心对称网格
- [x] 支持 `WaitingMarketPrice / WaitingRangeEntry / Active / Occupied` 状态
- [x] 价格回到区间后自动恢复挂单

### K9.3 多实例控制面与 TUI

- [x] 增加 `/instances` 与 `/instances/{symbol}/...` 控制面路由
- [x] TUI 启动先拉实例目录，再按默认 symbol 拉快照和建立实例作用域 WebSocket
- [x] TUI 支持切换当前实例并清空旧实例运行态
- [x] Market 页展示实例列表、当前实例和默认实例

### K9.4 收尾与验收

- [x] 跑通 `cargo test -p grid-platform-service --test multi_instance_control_plane`
- [x] 跑通 `cargo test -p grid-platform-tui --test instance_switching`
- [x] 跑通 `cargo test -p grid-platform-tui --test local_paper_e2e`
- [x] 跑通 `cargo test -p grid-platform-tui`
- [x] 更新 `README.md` 与本文件状态
- [x] 验收结论：K9 已验收通过

## 当前并行方式

- 按 `K10 -> K11` 单主线串行推进
- 多实例能力已并入主线，后续只补实例键、路径、事件和运行安全边界

## 完成后必做

- [x] 更新 `docs/plan.md` 中对应里程碑状态
- [x] 更新本文件中的任务勾选状态
- [x] 记录最近一次验证结果
