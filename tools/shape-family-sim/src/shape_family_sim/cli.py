from __future__ import annotations

import argparse
from collections.abc import Sequence

from .scenarios import get_scenario_prices, list_scenarios, parse_prices_csv
from .simulator import ShapeFamily, SimulationConfig, simulate_path


class ShapeFamilySimArgumentParser(argparse.ArgumentParser):
    def parse_args(self, args=None, namespace=None):
        parsed = super().parse_args(args, namespace)
        if parsed.list_scenarios:
            return parsed
        if bool(parsed.scenario) == bool(parsed.prices):
            self.error("exactly one of --scenario or --prices is required")
        return parsed


def build_parser() -> argparse.ArgumentParser:
    parser = ShapeFamilySimArgumentParser(
        prog="shape-family-sim",
        description="比较不同 shape family 在离散调仓近似下的收益、成本和换手。",
    )
    parser.add_argument("--scenario", help="使用内建价格路径。")
    parser.add_argument("--prices", help="使用逗号分隔的价格序列。")
    parser.add_argument(
        "--family",
        action="append",
        choices=[family.value for family in ShapeFamily],
        help="只运行指定 shape family，可重复传入。",
    )
    parser.add_argument(
        "--list-scenarios",
        action="store_true",
        help="列出内建场景后退出。",
    )
    parser.add_argument("--lower", type=float, default=90.0, help="价格带下沿。")
    parser.add_argument("--upper", type=float, default=110.0, help="价格带上沿。")
    parser.add_argument("--long-units", type=float, default=8.0, help="下沿最大多头单位。")
    parser.add_argument("--short-units", type=float, default=8.0, help="上沿最大空头单位。")
    parser.add_argument(
        "--notional-per-unit",
        type=float,
        default=375.0,
        help="每个 exposure unit 对应的名义金额。",
    )
    parser.add_argument(
        "--min-rebalance-units",
        type=float,
        default=0.5,
        help="触发一次调仓的最小目标变化。",
    )
    parser.add_argument(
        "--fee-rate",
        type=float,
        default=0.0002,
        help="每次调仓按成交金额估算的单边费率。",
    )
    return parser


def run(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.list_scenarios:
        for scenario in list_scenarios():
            print(f"{scenario.name}: {scenario.description}")
        return 0

    try:
        prices = get_prices(args)
        config = SimulationConfig(
            lower_price=args.lower,
            upper_price=args.upper,
            long_units=args.long_units,
            short_units=args.short_units,
            notional_per_unit=args.notional_per_unit,
            min_rebalance_units=args.min_rebalance_units,
            fee_rate=args.fee_rate,
        )
    except ValueError as exc:
        parser.error(str(exc))

    families = (
        [ShapeFamily(name) for name in args.family]
        if args.family
        else list(ShapeFamily)
    )
    results = [simulate_path(prices, family, config) for family in families]
    print_results(results)
    return 0


def get_prices(args: argparse.Namespace) -> list[float]:
    if args.scenario:
        return get_scenario_prices(args.scenario)
    if args.prices:
        return parse_prices_csv(args.prices)
    raise ValueError("exactly one of scenario or prices must be provided")


def print_results(results) -> None:
    print(
        "family     gross_pnl  fees    net_pnl  trades  turnover_units  final_exposure"
    )
    for result in results:
        print(
            f"{result.family.value:<10} "
            f"{result.gross_pnl:>9.2f} "
            f"{result.fees:>7.2f} "
            f"{result.net_pnl:>9.2f} "
            f"{result.trade_count:>7} "
            f"{result.turnover_units:>15.2f} "
            f"{result.final_exposure:>14.2f}"
        )


def main() -> None:
    raise SystemExit(run())
