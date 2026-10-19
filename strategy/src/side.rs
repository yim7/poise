//! 网格的方向与一侧的完整划分。
//!
//! 一个策略最多有两侧（设计第 2.1 节）：多头侧在价格下方，价格下跌时建多仓；
//! 空头侧在价格上方，价格上涨时建空仓。两侧的结构完全一样，只是价格和买卖方向
//! 对调（设计第 5.1 节）：
//!
//! | | 多头侧 | 空头侧 |
//! | --- | --- | --- |
//! | 建仓腿 | 买入 | 卖出 |
//! | 平仓腿 | 卖出 | 买入 |
//! | 启动顺序 | 从最高价往下 | 从最低价往上 |
//! | 平仓价 | 建仓价 + g | 建仓价 − g |
//! | 单边突破 | 向上突破只留主仓多单 | 向下突破只留主仓空单 |
//!
//! 「价格到达」也随方向对调：多头侧的建仓买单在价格跌到挂单价或更低时成交，
//! 空头侧的建仓卖单在价格涨到挂单价或更高时成交。
//!
//! 批次划分、格子循环、状态与规划对两侧是同一段代码。方向只在建价格和判方向时
//! 用得上，都收在这里。

use serde::{Deserialize, Serialize};

use crate::batch::BatchPlan;
use crate::config::SideConfig;
use crate::grid::GridPlan;
use crate::order::Side;
use crate::units::{Price, PriceRounding};

/// 网格的一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    /// 两侧，按配置字段的顺序。规整与校验按这个顺序进行。
    pub const ALL: [Direction; 2] = [Direction::Long, Direction::Short];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }

    /// 建仓腿的方向：多头买入建仓，空头卖出建仓。
    pub const fn entry_side(self) -> Side {
        match self {
            Self::Long => Side::Buy,
            Self::Short => Side::Sell,
        }
    }

    /// 平仓腿的方向：多头卖出平仓，空头买入平仓。
    pub const fn exit_side(self) -> Side {
        match self {
            Self::Long => Side::Sell,
            Self::Short => Side::Buy,
        }
    }

    /// 本侧持仓的符号：多头为正，空头为负。
    pub const fn sign(self) -> i64 {
        match self {
            Self::Long => 1,
            Self::Short => -1,
        }
    }

    /// 从启动端往本侧方向走时的价格符号：多头侧往下（减），空头侧往上（加）。
    ///
    /// 它与 [`Direction::sign`] 相反：持仓符号说的是仓位朝哪边，这个说的是价格往哪边走。
    pub const fn offset_sign(self) -> i64 {
        match self {
            Self::Long => -1,
            Self::Short => 1,
        }
    }

    /// 启动顺序从哪一端的价格开始：多头侧从最高价往下，空头侧从最低价往上。
    pub const fn start(self, low: Price, high: Price) -> Price {
        match self {
            Self::Long => high,
            Self::Short => low,
        }
    }

    /// 从启动端的价格偏移 `offset` 个最小单位。
    ///
    /// 偏移量不超过本侧的价格跨度，所以结果一定落在 `[L, H]` 里。
    pub const fn offset_price(self, start: Price, offset: i64) -> Price {
        Price::from_units(start.units() + self.offset_sign() * offset)
    }

    /// 行情 `market` 是否已经到达 `level`：到了就会让挂在 `level` 的建仓单成交。
    ///
    /// 多头侧的买单在价格跌到挂单价或更低时成交，空头侧的卖单在价格涨到挂单价
    /// 或更高时成交。
    pub const fn reached(self, level: Price, market: Price) -> bool {
        match self {
            Self::Long => market.units() <= level.units(),
            Self::Short => market.units() >= level.units(),
        }
    }

    /// 行情 `market` 是否已经到达平仓价 `level`：到了就会让挂在那里的平仓单成交。
    ///
    /// 和 [`Direction::reached`] 方向相反：多头侧的卖单在价格涨到挂单价或更高时成交，
    /// 空头侧的买平单在价格跌到挂单价或更低时成交。
    pub const fn exit_reached(self, level: Price, market: Price) -> bool {
        match self {
            Self::Long => market.units() >= level.units(),
            Self::Short => market.units() <= level.units(),
        }
    }

    /// 平仓价：多头侧在建仓价上加价差，空头侧减价差。超出可表示范围时返回 `None`。
    pub const fn exit_price(self, entry_price: Price, grid_spread: Price) -> Option<Price> {
        match self {
            Self::Long => entry_price.checked_add(grid_spread),
            Self::Short => entry_price.checked_sub(grid_spread),
        }
    }

    /// 平仓价规整到报价精度的方向：取让价差变大的那一边。
    ///
    /// 平仓价可能跨到步长更粗的价位上，按这个方向规整之后价差只会变大，不会变小。
    pub const fn exit_rounding(self) -> PriceRounding {
        match self {
            Self::Long => PriceRounding::Up,
            Self::Short => PriceRounding::Down,
        }
    }

    /// 建仓价规整到报价精度的方向：朝启动端。
    ///
    /// 建仓价逐格按各自价位的步长规整时，方向决定这一格什么时候被放行：朝启动端规整的
    /// 价格不会比理想位置更远离启动端，格子也就不会比理想位置更晚放行。方向与
    /// [`Direction::exit_rounding`] 相同，理由不同：那里是为了让价差只增不减。
    pub const fn entry_rounding(self) -> PriceRounding {
        match self {
            Self::Long => PriceRounding::Up,
            Self::Short => PriceRounding::Down,
        }
    }

    /// 界面上的叫法：做多侧、做空侧。报错用它开头，和表单里的分组对得上。
    pub const fn side_label(self) -> &'static str {
        match self {
            Self::Long => "做多侧",
            Self::Short => "做空侧",
        }
    }

    /// 单字简写：多、空。格子写成「多3」，仓位写成「多仓」，和页面一致。
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::Long => "多",
            Self::Short => "空",
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一侧的完整划分：规整后的配置、批次与格子。
///
/// 批次与格子里的价格都是这一侧的实际挂单价，不再需要按方向换算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidePlan {
    direction: Direction,
    config: SideConfig,
    batches: BatchPlan,
    grid: GridPlan,
}

impl SidePlan {
    pub(crate) fn new(
        direction: Direction,
        config: SideConfig,
        batches: BatchPlan,
        grid: GridPlan,
    ) -> Self {
        Self {
            direction,
            config,
            batches,
            grid,
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// 规整后的这一侧配置。
    pub fn config(&self) -> &SideConfig {
        &self.config
    }

    /// 批次划分；主仓比例为 0 时没有批次。
    pub fn batches(&self) -> &BatchPlan {
        &self.batches
    }

    pub fn grid(&self) -> &GridPlan {
        &self.grid
    }

    /// 本侧限速的单位数：`r > 0` 时是批数，`r = 0` 时是格数。
    pub fn unit_count(&self) -> usize {
        if self.config.core_ratio.is_zero() {
            self.grid.len()
        } else {
            self.batches.len()
        }
    }
}
