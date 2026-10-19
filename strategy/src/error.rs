//! 配置错误：规整、划分批次与格子、金额与盈利校验失败的原因。
//!
//! 一侧的错误都带上 `direction`：两侧的字段名和下标是一样的，不说清是哪一侧就没法改。
//! 只有账户级别的错误（taker 费率）不带，它对两侧都一样。
//!
//! 报错原样显示在预览页上：用中文写清是哪一侧、哪个参数（用表单上的符号 L、H、M、r、n、d、g、R），
//! 能改的说出怎么改。
//!
//! 单独成模块，是因为生成批次与格子的 [`crate::batch`]、[`crate::grid`] 也要报这些错；
//! 放在 [`crate::config`] 里就会让 config 和它们互相引用。

use chrono::Duration;

use crate::batch::BatchIndex;
use crate::grid::CellIndex;
use crate::side::Direction;
use crate::units::{Price, Quantity, Rate};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("标的 `{config}` 和交易所的 `{spec}` 不一致")]
    InstrumentMismatch { config: String, spec: String },
    #[error("至少启用一侧")]
    NoSidesConfigured,
    #[error("做多侧上限 H {long_high} 须低于做空侧下限 L {short_low}")]
    SidesOverlap { long_high: Price, short_low: Price },
    #[error("{side}：中性网格没有主仓，主仓比例 r 须为 0（现在是 {core_ratio}）", side = .direction.side_label())]
    CoreInNeutralGrid {
        direction: Direction,
        core_ratio: Rate,
    },
    #[error(
        "{side}：观点失效价 {price} 须在范围外（做多低于 L，做空高于 H），现在的边界是 {edge}",
        side = .direction.side_label()
    )]
    InvalidationPriceInsideRange {
        direction: Direction,
        price: Price,
        edge: Price,
    },
    #[error("{side}：失效后剩余 R {remaining} 超过主仓总量 {core_total}", side = .direction.side_label())]
    InvalidationRemainingAboveCore {
        direction: Direction,
        remaining: Quantity,
        core_total: Quantity,
    },
    #[error("{side}：中性网格失效后全部平仓，剩余 R 须为 0（现在是 {remaining}）", side = .direction.side_label())]
    InvalidationRemainingInNeutralGrid {
        direction: Direction,
        remaining: Quantity,
    },
    #[error("{side}：失效平仓滑点上限须大于 0、小于 100%（现在是 {max_slippage}）", side = .direction.side_label())]
    InvalidationSlippageOutOfRange {
        direction: Direction,
        max_slippage: Rate,
    },
    #[error("{side}：{field}不能为负（现在是 {price}）", side = .direction.side_label())]
    NegativePrice {
        direction: Direction,
        field: &'static str,
        price: Price,
    },
    #[error("{side}：最大仓位 M 不能为负（现在是 {max_position}）", side = .direction.side_label())]
    NegativeMaxPosition {
        direction: Direction,
        max_position: Quantity,
    },
    #[error("{side}：按报价精度规整后，下限 L {low} 须低于上限 H {high}", side = .direction.side_label())]
    PriceRangeNotAscending {
        direction: Direction,
        low: Price,
        high: Price,
    },
    #[error("{side}：{field} {price} 超出可表示的价格范围（报价精度 {price_tick}）", side = .direction.side_label())]
    PriceOutOfRange {
        direction: Direction,
        field: &'static str,
        price: Price,
        price_tick: Price,
    },
    #[error(
        "{side}：第 {batch_index} 批的价格（{price_low} 到 {price_high} 之间）超出可表示的价格范围",
        side = .direction.side_label()
    )]
    BatchPriceOutOfRange {
        direction: Direction,
        batch_index: BatchIndex,
        price_low: Price,
        price_high: Price,
    },
    #[error("{side}：网格间距 d {grid_spacing} 小于最小报价单位 {price_tick}", side = .direction.side_label())]
    GridSpacingBelowOneTick {
        direction: Direction,
        grid_spacing: Price,
        price_tick: Price,
    },
    #[error("{side}：网格价差 g {grid_spread} 小于最小报价单位 {price_tick}", side = .direction.side_label())]
    GridSpreadBelowOneTick {
        direction: Direction,
        grid_spread: Price,
        price_tick: Price,
    },
    #[error("{side}：最大仓位 M {max_position} 小于最小数量单位 {quantity_step}", side = .direction.side_label())]
    MaxPositionBelowOneStep {
        direction: Direction,
        max_position: Quantity,
        quantity_step: Quantity,
    },
    #[error("{side}：主仓比例 r 须在 0 到 1 之间（现在是 {core_ratio}）", side = .direction.side_label())]
    CoreRatioOutOfRange {
        direction: Direction,
        core_ratio: Rate,
    },
    #[error("{side}：限速间隔不能为负", side = .direction.side_label())]
    NegativeThrottleInterval {
        direction: Direction,
        interval: Duration,
    },
    #[error("{side}：主仓比例 r 大于 0 时，批数 n 至少为 1", side = .direction.side_label())]
    BatchCountOutOfRange {
        direction: Direction,
        batch_count: Option<u32>,
    },
    #[error("{side}：批数 n {batch_count} 超过上限 {max_batch_count}", side = .direction.side_label())]
    TooManyBatches {
        direction: Direction,
        batch_count: u32,
        max_batch_count: usize,
    },
    #[error(
        "{side}：需要 {cell_count} 格，超过上限 {max_cell_count} 格；加大网格间距 d",
        side = .direction.side_label()
    )]
    TooManyCells {
        direction: Direction,
        cell_count: usize,
        max_cell_count: usize,
    },
    #[error(
        "{side}：格子太多，每格数量不够最低下单金额（{short}{cell_index} 格在 {cell_price} 只有 {cell_quantity}，至少要 {required_quantity}）。这个范围最多约 {max_cell_count} 格：加大网格间距 d 或最大仓位 M",
        side = .direction.side_label(),
        short = .direction.short_label()
    )]
    CellBelowMinimumOrderNotional {
        direction: Direction,
        cell_index: CellIndex,
        cell_price: Price,
        cell_quantity: Quantity,
        required_quantity: Quantity,
        max_cell_count: usize,
    },
    #[error(
        "{side}：第 {batch_index} 批（{batch_price}）的区间 ({segment_low}, {segment_high}] 里没有格子；减小网格间距 d 或减少批数 n",
        side = .direction.side_label()
    )]
    BatchWithoutCell {
        direction: Direction,
        batch_index: BatchIndex,
        batch_price: Price,
        segment_low: Price,
        segment_high: Price,
    },
    #[error(
        "{side}：第 {batch_index} 批（{batch_price}）主仓只有 {core_quantity}，不够最低下单金额（至少要 {required_quantity}）；减少批数 n",
        side = .direction.side_label()
    )]
    BatchCoreBelowMinimumOrderNotional {
        direction: Direction,
        batch_index: BatchIndex,
        batch_price: Price,
        core_quantity: Quantity,
        required_quantity: Quantity,
    },
    #[error(
        "{side}：{short}{cell_index} 格（{entry_price}）加上价差 g {grid_spread} 后，平仓价超出可表示的价格范围",
        side = .direction.side_label(),
        short = .direction.short_label()
    )]
    ExitPriceOutOfRange {
        direction: Direction,
        cell_index: CellIndex,
        entry_price: Price,
        grid_spread: Price,
    },
    #[error(
        "{side}：网格价差 g {grid_spread} 扣掉两边的 taker 手续费（费率 {taker_fee_rate}）后不赚钱，至少要 {minimum_grid_spread}",
        side = .direction.side_label()
    )]
    GridSpreadNotProfitable {
        direction: Direction,
        grid_spread: Price,
        minimum_grid_spread: Price,
        taker_fee_rate: Rate,
    },
    #[error(
        "{side}：在 {entry_price} 找不到能覆盖两边 taker 手续费（费率 {taker_fee_rate}）的平仓价",
        side = .direction.side_label()
    )]
    GridSpreadUnreachable {
        direction: Direction,
        entry_price: Price,
        taker_fee_rate: Rate,
    },
    #[error("taker 费率 {taker_fee_rate} 不低于 1，任何网格都不赚钱")]
    TakerFeeRateTooHigh { taker_fee_rate: Rate },
}
