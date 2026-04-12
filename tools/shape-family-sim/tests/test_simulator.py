import unittest

from shape_family_sim.simulator import (
    ShapeFamily,
    SimulationConfig,
    shape_family_exponents,
    simulate_path,
)


class SimulatorTest(unittest.TestCase):
    def setUp(self) -> None:
        self.base_config = SimulationConfig(
            lower_price=90.0,
            upper_price=110.0,
            long_units=8.0,
            short_units=8.0,
            notional_per_unit=375.0,
            min_rebalance_units=0.5,
            fee_rate=0.0002,
        )

    def test_responsive_is_suppressed_for_small_center_chop(self) -> None:
        result = simulate_path(
            [99.0, 101.0] * 20,
            ShapeFamily.RESPONSIVE,
            self.base_config,
        )

        self.assertEqual(result.trade_count, 0)
        self.assertAlmostEqual(result.net_pnl, 0.0, places=6)

    def test_responsive_loses_less_than_linear_on_breakout(self) -> None:
        prices = [95.0, 97.0, 99.0, 101.0, 103.0, 105.0, 107.0, 109.0, 111.0, 113.0]

        linear = simulate_path(prices, ShapeFamily.LINEAR, self.base_config)
        responsive = simulate_path(prices, ShapeFamily.RESPONSIVE, self.base_config)

        self.assertGreater(responsive.net_pnl, linear.net_pnl)

    def test_half_band_chop_keeps_family_ordering(self) -> None:
        prices = [95.0, 105.0] * 20

        inertial = simulate_path(prices, ShapeFamily.INERTIAL, self.base_config)
        linear = simulate_path(prices, ShapeFamily.LINEAR, self.base_config)
        responsive = simulate_path(prices, ShapeFamily.RESPONSIVE, self.base_config)

        self.assertGreater(inertial.net_pnl, linear.net_pnl)
        self.assertGreater(linear.net_pnl, responsive.net_pnl)

    def test_shape_family_exponents_are_loaded_from_shared_file(self) -> None:
        exponents = shape_family_exponents()

        self.assertEqual(exponents["linear"], 1.0)
        self.assertEqual(exponents["inertial"], 0.65)
        self.assertEqual(exponents["responsive"], 1.6)
        self.assertEqual(ShapeFamily.INERTIAL.exponent, exponents["inertial"])
        self.assertEqual(ShapeFamily.RESPONSIVE.exponent, exponents["responsive"])

    def test_simulation_config_rejects_zero_capacity(self) -> None:
        with self.assertRaisesRegex(ValueError, "at least one capacity must be positive"):
            SimulationConfig(long_units=0.0, short_units=0.0)

    def test_simulation_config_rejects_invalid_band(self) -> None:
        with self.assertRaisesRegex(ValueError, "lower_price must be less than upper_price"):
            SimulationConfig(lower_price=100.0, upper_price=100.0)

    def test_simulation_config_rejects_negative_fee_rate(self) -> None:
        with self.assertRaisesRegex(ValueError, "fee_rate must not be negative"):
            SimulationConfig(fee_rate=-0.001)


if __name__ == "__main__":
    unittest.main()
