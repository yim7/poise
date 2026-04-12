from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Scenario:
    name: str
    prices: tuple[float, ...]
    description: str


SCENARIOS: dict[str, Scenario] = {
    "small-center-chop-x20": Scenario(
        name="small-center-chop-x20",
        prices=tuple([99.0, 101.0] * 20),
        description="中点附近小幅来回摆动 20 次。",
    ),
    "half-band-chop-x20": Scenario(
        name="half-band-chop-x20",
        prices=tuple([95.0, 105.0] * 20),
        description="半程位置来回摆动 20 次。",
    ),
    "edge-to-center-then-back-x10": Scenario(
        name="edge-to-center-then-back-x10",
        prices=tuple([90.0, 92.5, 95.0, 97.5, 100.0, 97.5, 95.0, 92.5] * 10),
        description="从下沿逐步回到中点，再回到下半区，重复 10 次。",
    ),
    "drift-up-then-back": Scenario(
        name="drift-up-then-back",
        prices=(
            90.0,
            92.0,
            94.0,
            96.0,
            98.0,
            100.0,
            102.0,
            104.0,
            106.0,
            108.0,
            110.0,
            108.0,
            106.0,
            104.0,
            102.0,
            100.0,
            98.0,
            96.0,
            94.0,
            92.0,
            90.0,
        ),
        description="从下沿单边上行到上沿，再原路回到下沿。",
    ),
    "one-way-breakout": Scenario(
        name="one-way-breakout",
        prices=(95.0, 97.0, 99.0, 101.0, 103.0, 105.0, 107.0, 109.0, 111.0, 113.0),
        description="从下半区穿过中点后继续向上突破出带。",
    ),
}


def list_scenarios() -> list[Scenario]:
    return list(SCENARIOS.values())


def get_scenario_prices(name: str) -> list[float]:
    try:
        return list(SCENARIOS[name].prices)
    except KeyError as exc:
        known = ", ".join(sorted(SCENARIOS))
        raise ValueError(f"unknown scenario '{name}', choose from: {known}") from exc


def parse_prices_csv(raw_prices: str) -> list[float]:
    prices = [part.strip() for part in raw_prices.split(",")]
    if any(not price for price in prices):
        raise ValueError("prices must be a comma-separated list of numbers")

    parsed = [float(price) for price in prices]
    if len(parsed) < 2:
        raise ValueError("prices must contain at least two points")

    return parsed
