//! 格子划分。
//!
//! 多头侧的格子买价从最高价 `H` 开始，每隔 `d` 一格，直到高于 `L` 的最后一格；
//! 每格卖价 = 买价 + `g`。格子按下标排列，0 是最靠近空隙（最高价）的一格，
//! 也是启动顺序里的第一个。

use serde::{Deserialize, Serialize};

use crate::units::{Price, Quantity, div_ceil};

/// 格子在网格中的下标，0 是最靠近空隙的一格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellIndex(pub usize);

impl std::fmt::Display for CellIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub index: CellIndex,
    pub buy_price: Price,
    pub sell_price: Price,
    pub quantity: Quantity,
}

/// 一个策略的格子划分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridPlan {
    cells: Vec<Cell>,
}

impl GridPlan {
    pub(crate) fn from_cells(cells: Vec<Cell>) -> Self {
        Self { cells }
    }

    /// 按价格从高到低排列。
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn cell(&self, index: CellIndex) -> Option<&Cell> {
        self.cells.get(index.0)
    }
}

/// 价格范围与间距决定多少格。
///
/// 先算格数、再决定要不要构造价格，避免极端配置在分配内存时就把进程拖死。
pub(crate) fn cell_count(high: Price, low: Price, spacing: Price) -> i64 {
    debug_assert!(high.units() > low.units());
    debug_assert!(spacing.units() > 0);
    let span = (high.units() - low.units()) as i128;
    div_ceil(span, spacing.units() as i128) as i64
}

/// 从 `H` 开始每隔 `spacing` 一格，直到高于 `low` 的最后一格。
pub(crate) fn cell_prices(high: Price, low: Price, spacing: Price) -> Vec<Price> {
    let count = cell_count(high, low, spacing);
    (0..count)
        .map(|step| Price::from_units(high.units() - step * spacing.units()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::cell_prices;
    use crate::units::Price;

    #[test]
    fn cell_prices_start_at_high_and_stop_above_low() {
        let prices = cell_prices(
            Price::parse("95000").unwrap(),
            Price::parse("75000").unwrap(),
            Price::parse("1000").unwrap(),
        );

        assert_eq!(prices.len(), 20);
        assert_eq!(prices[0], Price::parse("95000").unwrap());
        assert_eq!(prices[19], Price::parse("76000").unwrap());
        assert!(
            prices
                .iter()
                .all(|price| *price > Price::parse("75000").unwrap())
        );
    }

    #[test]
    fn cell_prices_cover_a_single_cell_when_spacing_exceeds_range() {
        let prices = cell_prices(
            Price::parse("95000").unwrap(),
            Price::parse("75000").unwrap(),
            Price::parse("100000").unwrap(),
        );

        assert_eq!(prices, vec![Price::parse("95000").unwrap()]);
    }
}
