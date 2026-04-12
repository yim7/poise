## shape-family-sim

用于比较 `linear / inertial / responsive` 在离散调仓近似下的收益、手续费和换手。

这个工具是低频研究工具，不是运行时回放器。它适合用来回答这类问题：

- 默认值为什么先选 `linear`
- `responsive` 在什么路径下更保守，什么时候可能反超
- 手续费和 `min_rebalance_units` 会不会改变 family 的相对表现

### 运行

先进入工具目录：

```bash
cd tools/shape-family-sim
```

查看内建场景：

```bash
uv run shape-family-sim --list-scenarios
```

直接跑一个内建路径：

```bash
uv run shape-family-sim --scenario one-way-breakout
```

或者直接传价格序列：

```bash
uv run shape-family-sim --prices 95,97,99,101,103,105
```

`--scenario` 和 `--prices` 必须二选一。

### 输出怎么看

输出列含义：

- `family`：曲线家族
- `gross_pnl`：未扣手续费的路径收益
- `fees`：按成交金额和 `fee_rate` 估算的手续费
- `net_pnl`：扣费后的净收益
- `trades`：触发调仓的次数
- `turnover_units`：累计调仓单位
- `final_exposure`：路径结束时的目标仓位

示例输出：

```bash
uv run shape-family-sim --scenario half-band-chop-x20
```

```text
family     gross_pnl  fees    net_pnl  trades  turnover_units  final_exposure
linear       5850.00   23.72   5826.28      40          316.00          -4.00
inertial     7456.18   30.23   7425.95      40          402.76          -5.10
responsive   3859.56   15.65   3843.91      40          208.48          -2.64
```

这组结果说明：

- `inertial` 参与度更高，收益和换手都更高
- `responsive` 仓位更轻，手续费和风险通常也更低
- `linear` 处在两者之间，适合做默认基线

### 常用参数

- `--scenario`：使用内建路径
- `--prices`：直接传逗号分隔的价格序列
- `--family`：只比较指定 family，可重复传入
- `--min-rebalance-units`：调仓门槛
- `--fee-rate`：单边费率
- `--lower` / `--upper`：价格带
- `--long-units` / `--short-units`：两侧最大仓位单位
- `--notional-per-unit`：每个单位对应的名义金额

### 内建场景

- `small-center-chop-x20`：中点附近小幅来回摆动 20 次
- `half-band-chop-x20`：半程位置来回摆动 20 次
- `edge-to-center-then-back-x10`：从下沿逐步回到中点，再回到下半区，重复 10 次
- `drift-up-then-back`：从下沿单边上行到上沿，再原路回到下沿
- `one-way-breakout`：从下半区穿过中点后继续向上突破出带

### 常见用法

对比默认配置下三种 family：

```bash
uv run shape-family-sim --scenario half-band-chop-x20
```

只比较 `linear` 和 `responsive`：

```bash
uv run shape-family-sim \
  --scenario one-way-breakout \
  --family linear \
  --family responsive
```

看手续费和调仓门槛对结果的影响：

```bash
uv run shape-family-sim \
  --scenario small-center-chop-x20 \
  --fee-rate 0.0005 \
  --min-rebalance-units 0.5
```

把价格带改成自己的研究区间：

```bash
uv run shape-family-sim \
  --scenario drift-up-then-back \
  --lower 80 \
  --upper 120 \
  --long-units 6 \
  --short-units 10
```

直接复现一条自定义路径：

```bash
uv run shape-family-sim \
  --prices 95,97,99,101,103,105,107,109,111,113 \
  --fee-rate 0.0002 \
  --min-rebalance-units 0.5
```

### 研究用例

验证中点附近小震荡时，`responsive` 会不会因为更轻仓而占优：

```bash
uv run shape-family-sim \
  --scenario small-center-chop-x20 \
  --family linear \
  --family responsive \
  --fee-rate 0.0002 \
  --min-rebalance-units 0.5
```

验证 breakout 路径里，`responsive` 是否亏得更少：

```bash
uv run shape-family-sim \
  --scenario one-way-breakout \
  --family linear \
  --family responsive
```

验证纯做空配置下，上半区中后段震荡时 `responsive` 是否可能反超：

```bash
uv run shape-family-sim \
  --prices 103,107,103,107,103,107,103,107 \
  --long-units 0 \
  --short-units 8 \
  --family linear \
  --family responsive \
  --fee-rate 0.0002 \
  --min-rebalance-units 0.5
```

验证纯做空配置下，更靠近中点时 `responsive` 是否会因为门槛抑制而掉队：

```bash
uv run shape-family-sim \
  --prices 101,103,101,103,101,103,101,103 \
  --long-units 0 \
  --short-units 8 \
  --family linear \
  --family responsive \
  --fee-rate 0.0002 \
  --min-rebalance-units 0.5
```

### 约束与边界

- `--scenario` 和 `--prices` 必须二选一
- 价格带必须满足 `lower < upper`
- `long_units` 和 `short_units` 不能为负，且至少一侧大于 `0`
- `notional_per_unit` 必须大于 `0`
- `fee_rate` 和 `min_rebalance_units` 不能为负
- 这是离散调仓近似，不包含撮合排队、部分成交、滑点分布和真实订单生命周期

参数不合法时，CLI 会直接返回可读错误，而不是 Python traceback。
