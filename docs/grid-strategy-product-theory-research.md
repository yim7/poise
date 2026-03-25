# 网格策略产品与学术理论调研

本文聚焦两件事：

- 业界成熟网格策略产品已经把哪些能力做成了稳定产品形态
- 学术界对网格策略本身的收益来源、失效条件和改良方向有哪些较可靠结论

资料检索时间：`2026-03-24`。

## 1. 结论先看

- 成熟产品的能力边界已经比较稳定，核心都围绕 `价格区间`、`网格间距`、`方向模式`、`触发/止盈止损`、`区间外处理`、`收益拆分展示` 展开。
- 现货与合约网格已经是标配；合约网格通常进一步支持 `Long / Short / Neutral` 和杠杆。
- 产品层面最常见的增强方向不是更复杂的数学，而是 `AI 参数建议`、`回测`、`运行中改参`、`策略市场`、`Trailing / Infinity / Expansion` 这类可运营能力。
- 学术界对“静态、固定区间、纯机械执行”的网格策略并不乐观。现有研究普遍认为，它的收益依赖震荡，长期仍有明显破产或大回撤风险。
- 研究界更看好的方向是 `动态重置`、`趋势识别`、`波动率自适应` 和 `资金/库存约束`。但这部分证据多数还是回测、预印本或特定市场样本，外推要保守。
- 对本项目最直接的价值，不是马上扩展更多 bot 形态，而是先把 `参数护栏`、`区间外状态机`、`合约风险语义`、`收益与风险拆分展示` 做扎实。

## 2. 业界成熟产品

### 2.1 代表产品

| 产品 | 已公开能力 | 产品成熟点 | 对本项目的启发 |
|---|---|---|---|
| Binance Trading Bots | 支持 `Spot Grid`、`Futures Grid`，并提供 `Bot Marketplace` 展示和复制热门参数 | 交易所内闭环完整，适合把“参数模板”和“策略分发”做成一等能力 | 后续如果做 Web UI，可以把“官方模板 / 推荐参数 / 热门策略”作为查询模型，而不是先做复杂策略编辑器 |
| OKX Trading Bots | 合约网格支持 `Long / Short / Neutral`、`Arithmetic / Geometric`、止盈止损；支持运行中编辑区间和网格数量 | 把策略模式、价格结构、风险退出、运行中维护放在同一工作流里，产品完整度高 | 当前仓库已经有固定区间梯子模型，后续可优先补 `Arithmetic / Geometric` 和“运行中改参”的边界设计 |
| Bybit Futures Grid Bot | 合约网格支持 `Long / Short / Neutral`；价格超出上下界时停止新单，允许等待回到区间后恢复或主动终止 | 对“区间外”行为定义清楚，同时把清算风险直接暴露给用户 | 本项目需要把 `WaitingRangeEntry / Active / MustPause / Terminated` 做成更清晰的状态机，而不是只用是否在挂单来暗示状态 |
| Pionex Grid / Infinity Grid | 常规网格支持 AI 参数建议、触发价、止损、止盈；`Infinity Grid` 处理单边上涨场景 | 机器人产品线最全，明确把“震荡网格”和“趋势网格”拆成不同产品，而不是混在一个策略里 | 本项目后续如果探索趋势场景，应把 `Infinity / Trailing` 做成独立策略族，而不是在当前固定区间网格里塞越来越多特例 |
| 3Commas Grid Bots | 提供 `120` 天回测、`AI Optimize`、`Trailing / Expansion`、`Stop Bot`，并支持多交易所 | 更像策略实验台，强调参数试错、回测和风控开关 | 对本项目最有价值的不是照搬 UI，而是尽快补 `回放 / 回测 / 参数评估` 的产品入口 |
| KuCoin Futures Grid | 合约网格面向新手提供 `long / short` 方向、`1-10x` 杠杆，并强调初始开仓不是满仓 | 在文案和默认行为上明显强调“部分初始仓位”和杠杆风险 | 本项目的实盘模式也应把“初始仓位比例”和“最大风险暴露”抬成显式参数，而不是只暴露区间和层数 |

### 2.2 成熟产品已经收敛出来的共性

#### 参数层

- 必有：`upper / lower`、`grid count` 或 `grid step`
- 常见：`Arithmetic / Geometric`
- 合约常见：`long / short / neutral`、`leverage`

#### 生命周期

- `立即启动` 或 `触发价启动`
- 超出区间后 `暂停等待回归` 或 `终止`
- 支持运行中改参，至少能改 `区间` 和 `网格数`

#### 风控层

- `Stop-loss`
- `Take-profit`
- `Stop bot`
- 合约模式下显式提示 `liquidation` 风险

#### 运营层

- AI 参数建议
- 策略市场或复制交易
- 回测或历史绩效视图
- 把 `grid profit`、`unrealized PnL`、`annualized` 分开显示

#### 单边行情应对

- `Infinity Grid`
- `Trailing Up / Trailing Down`
- `Expansion Up / Expansion Down`

这说明业界已经默认接受一个事实：静态固定区间网格只适合一部分行情，因此成熟产品都会提供某种“超出原区间后的处理机制”。

## 3. 学术界理论

### 3.1 研究现状

- 网格策略的系统化学术研究起步较晚。`2020` 年的相关论文还明确把双向网格交易视为此前几乎没有被严肃建模的对象。
- 公开研究可以分成三类：
  - 数学建模：把网格交易写成受边界约束的随机过程，研究破产、回撤和边界穿越
  - 策略改良：在传统网格上增加动态重置、趋势识别、波动率自适应
  - 工程回测：把机器学习、启发式调参、资金管理组合到网格执行里

### 3.2 相对可靠的理论结论

#### 结论 1：静态网格的收益来源是震荡，不是方向判断

固定区间网格本质上是在价格围绕某个范围来回波动时，反复做小额低买高卖。它并不天然拥有趋势预测能力。

`2025` 年的 DGT 论文直接把传统网格的期望收益拿出来分析，结论是在简单假设下，传统静态网格的期望收益接近零。这个结论很重要，因为它说明“机械分层挂单”本身不自动产生超额收益，收益还是来自市场结构而不是网格形式本身。

#### 结论 2：长期风险不能忽略，尤其是单边趋势和杠杆

`2020` 年 Taranto 与 Khan 的论文把网格交易问题和赌徒破产问题联系起来，结论不是“网格安全”，而是“相同风险水平下，短期可能比经典赌徒破产路径更有利，但长期仍然存在破产风险”。

这和产品侧经验完全一致：

- 价格单边脱离区间时，新单停止，资金效率下降
- 合约模式下会进一步叠加清算风险
- 如果还允许无限补仓或无限扩张网格，尾部风险会迅速放大

#### 结论 3：交易成本对网格有效性有硬约束

学术与产品侧在这一点上方向一致。DGT 论文指出过密网格会被交易成本侵蚀；Pionex 和 3Commas 的官方文档也都明确提醒，网格太密时净利润会被手续费吃掉。

对工程实现来说，这意味着：

- `profit_per_grid` 不能只大于 `0`
- 需要显式大于 `maker/taker fee * 2 + 预估滑点 + 最小价格/数量离散化损耗`

#### 结论 4：更强的结果通常来自“动态化”，不是静态网格本身

近年的正向结果大多来自三类改良：

- 动态重置网格中心或边界
- 用趋势分类器先决定 `long / short / neutral`
- 用波动率或机器学习模型自动调步长、层数、资金分配

但这里要区分“研究方向成立”和“已经得到广泛验证”：

- `GTSbot`、`Flexible Grid`、`DGT` 都报告了不错的回测结果
- 但它们大多是特定市场、特定时间窗、特定成本假设下的结果
- 目前还看不到足够强的跨市场、跨周期、长时间线上线验证

### 3.3 学术界提供给产品设计的真正价值

学术研究最值得拿来指导产品的，不是论文里报出的年化收益，而是下面这些结构化认识：

- 网格策略天然依赖边界条件，必须把 `边界穿越` 当成一等事件
- 资金利用率、库存累积、回撤和破产概率，应该和成交收益一起建模
- 震荡、趋势、跳空这三类市场状态，不应该共享一套默认参数
- 动态重置是独立策略机制，不是对静态网格打一个补丁

## 4. 对本项目的直接启发

### 4.1 当前最值得先做的事情

1. 先补参数护栏。
   当前 `K11` 已经列出这项工作，建议把下面这些约束作为首批硬规则：
   - 单格理论毛利必须覆盖 `双边手续费 + 预估滑点`
   - 单格价格差必须覆盖 `tick size` 离散化误差
   - 单格下单量必须覆盖 `lot size / min notional`
   - `grid_levels`、`max_position_notional`、杠杆三者要有组合上限

2. 把区间外状态机做清楚。
   当前仓库已经有 `WaitingMarketPrice / WaitingRangeEntry / Active / Occupied`，后续建议再补：
   - 区间外停摆原因
   - 恢复条件
   - 自动恢复还是人工确认
   - 是否允许动态重置

3. 把合约风险语义从“执行风险”提升到“策略风险”。
   至少需要显式暴露：
   - 杠杆
   - 清算距离
   - 当前库存占用
   - 连续补单次数
   - 最近一次暂停原因

4. 收益展示要拆开。
   不要只给总盈亏，至少拆成：
   - `grid_profit`
   - `unrealized_pnl`
   - `fees`
   - `inventory_exposure`
   - `out_of_range_duration`

### 4.2 不建议现在就做的事情

- 不建议马上把 `Infinity Grid`、`Trailing`、`Expansion` 混进当前固定区间策略
- 不建议先做“参数很多的万能策略页”
- 不建议把预印本里的高收益结果直接当成交付目标

原因很直接：当前主线还是 `K10 -> K11`，最缺的是运行安全、参数护栏和状态解释，而不是策略种类数量。

### 4.3 建议的探索顺序

1. 先完成 `K10` 值守硬化。
2. 在 `K11` 先补参数护栏、风险动作语义、状态解释和复盘链路。
3. 等当前固定区间网格在实盘约束下站稳，再单开一条探索线验证：
   - `Arithmetic / Geometric`
   - 动态重置网格
   - 趋势型 `Infinity / Trailing`
   - 参数推荐与回测视图

## 5. 证据强弱判断

| 结论类型 | 当前证据强度 | 说明 |
|---|---|---|
| 主流产品都具备哪些能力 | 强 | 直接来自交易所和 bot 平台官方文档 |
| 静态网格依赖震荡、长期有明显尾部风险 | 中到强 | 有论文、论文摘要和博士论文支持，也与产品设计一致 |
| 动态重置或机器学习一定能长期跑赢 | 弱到中 | 多数还是回测、预印本或单市场样本，不能直接当成稳定事实 |

## 6. 参考资料

### 6.1 产品文档

- [Binance Academy: Your Guide to Binance Trading Bots](https://www.binance.com/en/academy/articles/your-guide-to-binance-trading-bots)
- [OKX: How do I manually set up futures grid trading bot?](https://www.okx.com/en-us/help/how-do-i-manually-set-up-futures-grid-trading-bot)
- [OKX: What's the Spot Grid bot and how do I use it?](https://www.okx.com/en-us/help/whats-the-spot-grid-bot-and-how-to-use-it)
- [Bybit: Introduction to Futures Grid Bot on Bybit](https://www.bybit.com/en/help-center/article?id=000001825&language=en_US)
- [Pionex: How to set up the parameters for my first grid bot?](https://www.pionex.com/blog/knowledge-base/how-to-set-up-the-parameters-for-my-first-grid-bot/)
- [Pionex: Infinity Grid](https://www.pionex.com/blog/pionex-infinity-grid-bot/)
- [3Commas: Grid bots: Main settings and options](https://help.3commas.io/en/articles/7932030-grid-bots-main-settings-and-options)
- [KuCoin: Getting Started with Futures Grid: Beginners Tutorial](https://www.kucoin.com/support/10560658630681)

### 6.2 学术与研究资料

- [Taranto, Khan, 2020, Gambler’s ruin problem and bi-directional grid constrained trading and investment strategies](https://www.businessperspectives.org/index.php/publishing-policies2/gambler-s-ruin-problem-and-bi-directional-grid-constrained-trading-and-investment-strategies)
- [Taranto, 2022, Bi-directional grid constrained stochastic processes and their applications in mathematical finance](https://research.usq.edu.au/item/q7q62/bi-directional-grid-constrained-stochastic-processes-and-their-applications-in-mathematical-finance)
- [Chen, Chen, Jang, 2025, Dynamic Grid Trading Strategy: From Zero Expectation to Market Outperformance](https://arxiv.org/abs/2506.11921)
- [Yeh, Hsieh, Huang, 2022, Newly Developed Flexible Grid Trading Model Combined ANN and SSO algorithm](https://arxiv.org/abs/2211.12839)
- [Rundo et al., 2019, Grid Trading System Robot (GTSbot): A Novel Mathematical Algorithm for Trading FX Market](https://www.mdpi.com/2076-3417/9/9/1796)
