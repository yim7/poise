from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from functools import cache
import json
import math
from pathlib import Path


class ShapeFamily(str, Enum):
    LINEAR = "linear"
    INERTIAL = "inertial"
    RESPONSIVE = "responsive"

    @property
    def exponent(self) -> float:
        return shape_family_exponents()[self.value]


@dataclass(frozen=True)
class SimulationConfig:
    lower_price: float = 90.0
    upper_price: float = 110.0
    long_units: float = 8.0
    short_units: float = 8.0
    notional_per_unit: float = 375.0
    min_rebalance_units: float = 0.5
    fee_rate: float = 0.0002

    def __post_init__(self) -> None:
        if not math.isfinite(self.lower_price) or not math.isfinite(self.upper_price):
            raise ValueError("lower_price and upper_price must be finite")
        if self.lower_price >= self.upper_price:
            raise ValueError("lower_price must be less than upper_price")
        if self.band_center <= 0.0:
            raise ValueError("band center must be positive")
        if not math.isfinite(self.long_units) or not math.isfinite(self.short_units):
            raise ValueError("long_units and short_units must be finite")
        if self.long_units < 0.0 or self.short_units < 0.0:
            raise ValueError("capacities must not be negative")
        if self.long_units + self.short_units <= 0.0:
            raise ValueError("at least one capacity must be positive")
        if not math.isfinite(self.notional_per_unit) or self.notional_per_unit <= 0.0:
            raise ValueError("notional_per_unit must be positive")
        if not math.isfinite(self.min_rebalance_units):
            raise ValueError("min_rebalance_units must be finite")
        if self.min_rebalance_units < 0.0:
            raise ValueError("min_rebalance_units must not be negative")
        if not math.isfinite(self.fee_rate):
            raise ValueError("fee_rate must be finite")
        if self.fee_rate < 0.0:
            raise ValueError("fee_rate must not be negative")

    @property
    def band_center(self) -> float:
        return (self.lower_price + self.upper_price) / 2.0

    @property
    def half_band(self) -> float:
        return (self.upper_price - self.lower_price) / 2.0

    @property
    def base_qty_per_unit(self) -> float:
        return self.notional_per_unit / self.band_center

    @property
    def span(self) -> float:
        return (self.long_units + self.short_units) / 2.0

    @property
    def bias(self) -> float:
        return (self.long_units - self.short_units) / 2.0


@dataclass(frozen=True)
class SimulationResult:
    family: ShapeFamily
    gross_pnl: float
    fees: float
    net_pnl: float
    trade_count: int
    turnover_units: float
    final_exposure: float


@cache
def shape_family_parameter_path() -> Path:
    current = Path(__file__).resolve()
    for parent in current.parents:
        candidate = parent / "core" / "shape_family_exponents.json"
        if candidate.exists():
            return candidate
    raise RuntimeError("could not locate core/shape_family_exponents.json from shape-family-sim")


@cache
def shape_family_exponents() -> dict[str, float]:
    raw = json.loads(shape_family_parameter_path().read_text(encoding="utf-8"))
    return {
        "linear": float(raw["linear"]),
        "inertial": float(raw["inertial"]),
        "responsive": float(raw["responsive"]),
    }


def desired_exposure(price: float, family: ShapeFamily, config: SimulationConfig) -> float:
    position = ((price - config.band_center) / config.half_band)
    clamped = max(-1.0, min(1.0, position))
    magnitude = abs(clamped) ** family.exponent
    shape_value = -magnitude if clamped >= 0.0 else magnitude
    return config.bias + config.span * shape_value


def simulate_path(
    prices: list[float],
    family: ShapeFamily,
    config: SimulationConfig,
) -> SimulationResult:
    if len(prices) < 2:
        raise ValueError("prices must contain at least two points")

    exposure = 0.0
    quantity = 0.0
    gross_pnl = 0.0
    fees = 0.0
    trade_count = 0
    turnover_units = 0.0

    for index, price in enumerate(prices):
        target_exposure = desired_exposure(price, family, config)
        gap = target_exposure - exposure
        if abs(gap) >= config.min_rebalance_units - 1e-12:
            delta_quantity = gap * config.base_qty_per_unit
            fees += abs(delta_quantity) * price * config.fee_rate
            quantity += delta_quantity
            exposure = target_exposure
            trade_count += 1
            turnover_units += abs(gap)

        if index + 1 < len(prices):
            next_price = prices[index + 1]
            gross_pnl += quantity * (next_price - price)

    return SimulationResult(
        family=family,
        gross_pnl=gross_pnl,
        fees=fees,
        net_pnl=gross_pnl - fees,
        trade_count=trade_count,
        turnover_units=turnover_units,
        final_exposure=exposure,
    )
