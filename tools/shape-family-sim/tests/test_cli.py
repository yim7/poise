import contextlib
import io
import unittest

from shape_family_sim.cli import build_parser, run


class CliTest(unittest.TestCase):
    def test_parser_rejects_scenario_and_prices_together(self) -> None:
        parser = build_parser()

        with self.assertRaises(SystemExit):
            parser.parse_args(
                [
                    "--scenario",
                    "small-center-chop-x20",
                    "--prices",
                    "99,101,99",
                ]
            )

    def test_list_scenarios_does_not_require_price_source(self) -> None:
        buffer = io.StringIO()

        with contextlib.redirect_stdout(buffer):
            exit_code = run(["--list-scenarios"])

        output = buffer.getvalue()
        self.assertEqual(exit_code, 0)
        self.assertIn("one-way-breakout", output)

    def test_run_prints_summary_table(self) -> None:
        buffer = io.StringIO()

        with contextlib.redirect_stdout(buffer):
            exit_code = run(
                [
                    "--scenario",
                    "one-way-breakout",
                    "--min-rebalance-units",
                    "0.5",
                    "--fee-rate",
                    "0.0002",
                ]
            )

        output = buffer.getvalue()
        self.assertEqual(exit_code, 0)
        self.assertIn("family", output)
        self.assertIn("linear", output)
        self.assertIn("responsive", output)

    def test_run_rejects_unknown_scenario_as_parser_error(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()

        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as context:
                run(["--scenario", "does-not-exist"])

        self.assertEqual(context.exception.code, 2)
        self.assertIn("unknown scenario", stderr.getvalue())

    def test_run_rejects_invalid_prices_as_parser_error(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()

        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as context:
                run(["--prices", "99,foo,101"])

        self.assertEqual(context.exception.code, 2)
        self.assertIn("could not convert", stderr.getvalue())

    def test_run_rejects_invalid_band_as_parser_error(self) -> None:
        stdout = io.StringIO()
        stderr = io.StringIO()

        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as context:
                run(
                    [
                        "--scenario",
                        "one-way-breakout",
                        "--lower",
                        "100",
                        "--upper",
                        "100",
                    ]
                )

        self.assertEqual(context.exception.code, 2)
        self.assertIn("lower_price must be less than upper_price", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
