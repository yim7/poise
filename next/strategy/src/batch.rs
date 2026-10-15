//! 批次划分（主仓）。
//!
//! 多头侧把 `[L, H]` 均分为 `n` 段，段宽 `w = (H − L) / n`，第 k 批的批次价是
//! `H − k × w`，对应的段是 `(H − (k+1)w, H − k × w]`。每批数量把 `M` 按数量
//! 最小单位平均分成 n 份，除不尽的余数依次多给靠近空隙的批次；每批的主仓
//! `C = ceil(r × q)`，网格部分 `G = q − C`。
//!
//! 批次按下标排列，0 是最靠近空隙（价格最高）的一批，也是启动顺序里的第一个。

use serde::{Deserialize, Serialize};

use crate::grid::CellIndex;
use crate::units::{Price, Quantity, div_round};

/// 批次在划分中的下标，0 是最靠近空隙的一批。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BatchIndex(pub usize);

impl std::fmt::Display for BatchIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// 一批主仓：批次价、总量、主仓部分和跟随它的格子。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub index: BatchIndex,
    /// 批次价，本批主仓买单挂在这里，不追高。
    pub price: Price,
    /// 本批总量 `q`。
    pub quantity: Quantity,
    /// 主仓部分 `C = ceil(r × q)`，按数量最小单位向上取整。
    pub core_quantity: Quantity,
    /// 网格部分 `G = q − C`，平均分给跟随本批的格子。
    pub grid_quantity: Quantity,
    /// 跟随本批的格子下标，按价格从高到低，也就是从靠近批次价的一端开始。
    pub cells: Vec<CellIndex>,
}

/// 一个策略的批次划分。主仓比例为 0 时没有批次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BatchPlan {
    batches: Vec<Batch>,
}

impl BatchPlan {
    pub(crate) fn from_batches(batches: Vec<Batch>) -> Self {
        Self { batches }
    }

    /// 按价格从高到低排列。
    pub fn batches(&self) -> &[Batch] {
        &self.batches
    }

    pub fn len(&self) -> usize {
        self.batches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn batch(&self, index: BatchIndex) -> Option<&Batch> {
        self.batches.get(index.0)
    }
}

/// 第 k 批的批次价：`H − k × (H − L) / n`，按报价精度规整一次。
///
/// 段宽 `w = (H − L) / n` 除不尽时，把余数分摊到各段上，而不是全部留给最后一段。
/// 中间量保留整数分子分母（`(H × n − k × (H − L)) / n`），只在最后规整一次：
/// 先把分段偏移取整、再按报价精度规整，会多做一次取整，把价格推到相邻的报价单位上，
/// 从而改变这个价格落在哪一批。
///
/// 规整后可能有两批落在同一个价格上，那种配置会被「每批都要有格子」的校验拒绝。
/// 规整结果超出可表示范围时返回 `None`。
pub(crate) fn batch_price(
    high: Price,
    low: Price,
    count: u32,
    index: usize,
    tick: Price,
) -> Option<Price> {
    debug_assert!(count > 0, "batch count must be positive");
    debug_assert!(high.units() > low.units(), "the range must ascend");
    debug_assert!(index < count as usize, "the index must be inside the range");
    debug_assert!(tick.units() > 0, "price tick must be positive");
    let high_units = i128::from(high.units());
    let span = high_units - i128::from(low.units());
    let count = i128::from(count);
    let tick = i128::from(tick.units());
    // 分子恒为正：`span ≤ H`，所以 `H × n − k × span ≥ H`。
    let numerator = high_units * count - span * index as i128;
    let steps = div_round(numerator, count * tick);
    i64::try_from(steps * tick).ok().map(Price::from_units)
}

#[cfg(test)]
mod tests {
    use super::batch_price;
    use crate::units::Price;

    /// 报价精度为一个报价单位，也就是常见的整数报价。
    fn unit_tick() -> Price {
        Price::parse("1").unwrap()
    }

    fn price(high: Price, low: Price, count: u32, index: usize) -> Price {
        batch_price(high, low, count, index, unit_tick()).unwrap()
    }

    #[test]
    fn batch_prices_split_the_range_into_equal_segments() {
        let high = Price::parse("95000").unwrap();
        let low = Price::parse("75000").unwrap();

        assert_eq!(price(high, low, 4, 0), high);
        assert_eq!(price(high, low, 4, 1), Price::parse("90000").unwrap());
        assert_eq!(price(high, low, 4, 2), Price::parse("85000").unwrap());
        assert_eq!(price(high, low, 4, 3), Price::parse("80000").unwrap());
        assert!(price(high, low, 4, 3) > low);
    }

    #[test]
    fn batch_prices_share_the_remainder_instead_of_giving_it_to_the_last_batch() {
        let high = Price::parse("95000").unwrap();
        let low = Price::parse("75000").unwrap();

        // 20000 / 3 除不尽：段宽在 6666 和 6667 之间分摊，最后一批仍在范围内。
        assert_eq!(price(high, low, 3, 1), Price::parse("88333").unwrap());
        assert_eq!(price(high, low, 3, 2), Price::parse("81667").unwrap());
        assert!(price(high, low, 3, 2) > low);
        // 余数全部留给最后一段的话，最后一批会落到 81667 以下。
        assert!(price(high, low, 3, 2) > Price::parse("81666").unwrap());
    }

    /// 精确批次价是 `174.999999988`，报价精度 `0.00000002` 下应当规整成
    /// `174.99999998`。先把偏移取整到 `1e-8` 得到 `174.99999999`，再规整就成了
    /// `175`：多做一次取整会把批次价推高一整个报价单位。
    #[test]
    fn a_batch_price_is_rounded_to_the_tick_only_once() {
        let high = Price::parse("200").unwrap();
        let low = Price::parse("74.99999994").unwrap();
        let tick = Price::parse("0.00000002").unwrap();

        assert_eq!(
            batch_price(high, low, 5, 1, tick).unwrap(),
            Price::parse("174.99999998").unwrap()
        );
        assert!(batch_price(high, low, 5, 1, tick).unwrap() < Price::parse("175").unwrap());
    }

    /// 规整结果超出可表示范围时返回 `None`，不 panic 也不饱和。
    #[test]
    fn reports_batch_prices_that_are_not_representable() {
        // 第 0 批的批次价是 `H`，按一个报价单位规整后落到 `92233720369`，
        // 它的最小单位超过 `i64::MAX`。
        let high = Price::parse("92233720368.54775807").unwrap();
        let low = Price::parse("92233720367").unwrap();

        assert_eq!(batch_price(high, low, 2, 0, unit_tick()), None);
    }
}
