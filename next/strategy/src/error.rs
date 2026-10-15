//! 配置错误：规整、划分批次与格子、金额与盈利校验失败的原因。
//!
//! 单独成模块，是因为生成批次与格子的 [`crate::batch`]、[`crate::grid`] 也要报这些错；
//! 放在 [`crate::config`] 里就会让 config 和它们互相引用。

use chrono::Duration;

use crate::batch::BatchIndex;
use crate::grid::CellIndex;
use crate::units::{Price, Quantity, Rate};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("config instrument `{config}` does not match the exchange instrument `{spec}`")]
    InstrumentMismatch { config: String, spec: String },
    #[error("{field} must not be negative, got {price}")]
    NegativePrice { field: &'static str, price: Price },
    #[error("max position must not be negative, got {max_position}")]
    NegativeMaxPosition { max_position: Quantity },
    #[error("price range must be ascending after rounding, got {low} and {high}")]
    PriceRangeNotAscending { low: Price, high: Price },
    #[error("{field} {price} is out of the representable price range at tick {price_tick}")]
    PriceOutOfRange {
        field: &'static str,
        price: Price,
        price_tick: Price,
    },
    #[error(
        "batch {batch_index} price between {price_low} and {price_high} is out of the representable price range"
    )]
    BatchPriceOutOfRange {
        batch_index: BatchIndex,
        price_low: Price,
        price_high: Price,
    },
    #[error("grid spacing {grid_spacing} must be at least one price tick {price_tick}")]
    GridSpacingBelowOneTick {
        grid_spacing: Price,
        price_tick: Price,
    },
    #[error("grid spread {grid_spread} must be at least one price tick {price_tick}")]
    GridSpreadBelowOneTick {
        grid_spread: Price,
        price_tick: Price,
    },
    #[error("max position {max_position} is below one quantity step {quantity_step}")]
    MaxPositionBelowOneStep {
        max_position: Quantity,
        quantity_step: Quantity,
    },
    #[error("core ratio {core_ratio} must be between 0 and 1 inclusive")]
    CoreRatioOutOfRange { core_ratio: Rate },
    #[error("throttle interval must not be negative, got {interval:?}")]
    NegativeThrottleInterval { interval: Duration },
    #[error("core ratio is above 0, so the batch count must be at least 1, got {batch_count:?}")]
    BatchCountOutOfRange { batch_count: Option<u32> },
    #[error("batch count {batch_count} is above the supported maximum {max_batch_count}")]
    TooManyBatches {
        batch_count: u32,
        max_batch_count: usize,
    },
    #[error("grid needs {cell_count} cells, more than the supported maximum {max_cell_count}")]
    TooManyCells {
        cell_count: usize,
        max_cell_count: usize,
    },
    #[error(
        "cell {cell_index} at {cell_price} holds {cell_quantity}, below the {required_quantity} required for the minimum order amount; at most {max_cell_count} cells fit in this range"
    )]
    CellBelowMinimumOrderNotional {
        cell_index: CellIndex,
        cell_price: Price,
        cell_quantity: Quantity,
        required_quantity: Quantity,
        max_cell_count: usize,
    },
    #[error(
        "batch {batch_index} at {batch_price} has no cell in its segment ({segment_low}, {segment_high}]; use a smaller grid spacing or fewer batches"
    )]
    BatchWithoutCell {
        batch_index: BatchIndex,
        batch_price: Price,
        segment_low: Price,
        segment_high: Price,
    },
    #[error(
        "batch {batch_index} at {batch_price} buys {core_quantity} as core, below the {required_quantity} required for the minimum order amount; use fewer batches"
    )]
    BatchCoreBelowMinimumOrderNotional {
        batch_index: BatchIndex,
        batch_price: Price,
        core_quantity: Quantity,
        required_quantity: Quantity,
    },
    #[error(
        "cell {cell_index} at {buy_price} plus the grid spread {grid_spread} is a price out of range"
    )]
    SellPriceOutOfRange {
        cell_index: CellIndex,
        buy_price: Price,
        grid_spread: Price,
    },
    #[error(
        "grid spread {grid_spread} does not stay profitable after taker fees {taker_fee_rate} on both legs; at least {minimum_grid_spread} is required"
    )]
    GridSpreadNotProfitable {
        grid_spread: Price,
        minimum_grid_spread: Price,
        taker_fee_rate: Rate,
    },
    #[error(
        "no price at {buy_price} can cover taker fees {taker_fee_rate} on both legs within the representable range"
    )]
    GridSpreadUnreachable {
        buy_price: Price,
        taker_fee_rate: Rate,
    },
    #[error("taker fee rate {taker_fee_rate} is at least 1, so no grid can be profitable")]
    TakerFeeRateTooHigh { taker_fee_rate: Rate },
}
