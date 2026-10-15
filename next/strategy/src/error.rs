//! 配置错误：规整、划分批次与格子、金额与盈利校验失败的原因。
//!
//! 一侧的错误都带上 `direction`：两侧的字段名和下标是一样的，不说清是哪一侧就没法改。
//! 只有账户级别的错误（taker 费率）不带，它对两侧都一样。
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
    #[error("config instrument `{config}` does not match the exchange instrument `{spec}`")]
    InstrumentMismatch { config: String, spec: String },
    #[error("a strategy needs at least one side: set `long`, `short`, or both")]
    NoSidesConfigured,
    #[error(
        "the long side must stay below the short side, but its high {long_high} is not below the short side low {short_low}"
    )]
    SidesOverlap { long_high: Price, short_low: Price },
    #[error(
        "a neutral grid has no core, but the {direction} side sets a core ratio of {core_ratio}; use a one-sided grid to keep a core"
    )]
    CoreInNeutralGrid {
        direction: Direction,
        core_ratio: Rate,
    },
    #[error("{direction} {field} must not be negative, got {price}")]
    NegativePrice {
        direction: Direction,
        field: &'static str,
        price: Price,
    },
    #[error("{direction} max position must not be negative, got {max_position}")]
    NegativeMaxPosition {
        direction: Direction,
        max_position: Quantity,
    },
    #[error("{direction} price range must be ascending after rounding, got {low} and {high}")]
    PriceRangeNotAscending {
        direction: Direction,
        low: Price,
        high: Price,
    },
    #[error(
        "{direction} {field} {price} is out of the representable price range at tick {price_tick}"
    )]
    PriceOutOfRange {
        direction: Direction,
        field: &'static str,
        price: Price,
        price_tick: Price,
    },
    #[error(
        "{direction} batch {batch_index} price between {price_low} and {price_high} is out of the representable price range"
    )]
    BatchPriceOutOfRange {
        direction: Direction,
        batch_index: BatchIndex,
        price_low: Price,
        price_high: Price,
    },
    #[error("{direction} grid spacing {grid_spacing} must be at least one price tick {price_tick}")]
    GridSpacingBelowOneTick {
        direction: Direction,
        grid_spacing: Price,
        price_tick: Price,
    },
    #[error("{direction} grid spread {grid_spread} must be at least one price tick {price_tick}")]
    GridSpreadBelowOneTick {
        direction: Direction,
        grid_spread: Price,
        price_tick: Price,
    },
    #[error("{direction} max position {max_position} is below one quantity step {quantity_step}")]
    MaxPositionBelowOneStep {
        direction: Direction,
        max_position: Quantity,
        quantity_step: Quantity,
    },
    #[error("{direction} core ratio {core_ratio} must be between 0 and 1 inclusive")]
    CoreRatioOutOfRange {
        direction: Direction,
        core_ratio: Rate,
    },
    #[error("{direction} throttle interval must not be negative, got {interval:?}")]
    NegativeThrottleInterval {
        direction: Direction,
        interval: Duration,
    },
    #[error(
        "{direction} core ratio is above 0, so the batch count must be at least 1, got {batch_count:?}"
    )]
    BatchCountOutOfRange {
        direction: Direction,
        batch_count: Option<u32>,
    },
    #[error(
        "{direction} batch count {batch_count} is above the supported maximum {max_batch_count}"
    )]
    TooManyBatches {
        direction: Direction,
        batch_count: u32,
        max_batch_count: usize,
    },
    #[error(
        "{direction} grid needs {cell_count} cells, more than the supported maximum {max_cell_count}"
    )]
    TooManyCells {
        direction: Direction,
        cell_count: usize,
        max_cell_count: usize,
    },
    #[error(
        "{direction} cell {cell_index} at {cell_price} holds {cell_quantity}, below the {required_quantity} required for the minimum order amount; at most {max_cell_count} cells fit in this range"
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
        "{direction} batch {batch_index} at {batch_price} has no cell in its segment ({segment_low}, {segment_high}]; use a smaller grid spacing or fewer batches"
    )]
    BatchWithoutCell {
        direction: Direction,
        batch_index: BatchIndex,
        batch_price: Price,
        segment_low: Price,
        segment_high: Price,
    },
    #[error(
        "{direction} batch {batch_index} at {batch_price} buys {core_quantity} as core, below the {required_quantity} required for the minimum order amount; use fewer batches"
    )]
    BatchCoreBelowMinimumOrderNotional {
        direction: Direction,
        batch_index: BatchIndex,
        batch_price: Price,
        core_quantity: Quantity,
        required_quantity: Quantity,
    },
    #[error(
        "{direction} cell {cell_index} at {entry_price} with the grid spread {grid_spread} has no exit price in the representable range"
    )]
    ExitPriceOutOfRange {
        direction: Direction,
        cell_index: CellIndex,
        entry_price: Price,
        grid_spread: Price,
    },
    #[error(
        "{direction} grid spread {grid_spread} does not stay profitable after taker fees {taker_fee_rate} on both legs; at least {minimum_grid_spread} is required"
    )]
    GridSpreadNotProfitable {
        direction: Direction,
        grid_spread: Price,
        minimum_grid_spread: Price,
        taker_fee_rate: Rate,
    },
    #[error(
        "{direction} has no price at {entry_price} that can cover taker fees {taker_fee_rate} on both legs within the representable range"
    )]
    GridSpreadUnreachable {
        direction: Direction,
        entry_price: Price,
        taker_fee_rate: Rate,
    },
    #[error("taker fee rate {taker_fee_rate} is at least 1, so no grid can be profitable")]
    TakerFeeRateTooHigh { taker_fee_rate: Rate },
}
