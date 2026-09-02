//! Shared deterministic replay primitives and a curve-immediate benchmark.
//!
//! [`run_replay`] applies `strategy::desired_exposure` directly at each price sample and assumes an
//! immediate full taker fill once `min_rebalance_units` is reached. That benchmark does not run the
//! engine manager, reconciler, risk-acquisition gate, executor lifecycle, or fresh-session logic,
//! so it must not be treated as a complete strategy or decision replay. Fee conversion and replay
//! exchange-rule validation are shared with the engine decision replay.

use serde::{Deserialize, Serialize};

use crate::instrument::{Amount, InstrumentSpec, PositionBasis, PositionQty};
use crate::strategy::{TrackConfig, desired_exposure, validate_config};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFeeRates {
    pub maker_fee_rate: f64,
    pub taker_fee_rate: f64,
}

impl ReplayFeeRates {
    pub fn validate(self) -> Result<(), String> {
        validate_fee_rate("maker_fee_rate", self.maker_fee_rate)?;
        validate_fee_rate("taker_fee_rate", self.taker_fee_rate)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayInput {
    pub prices: Vec<f64>,
    pub initial_native_quantity: f64,
    pub leverage: u32,
    pub track_config: TrackConfig,
    pub instrument_spec: InstrumentSpec,
    pub fee_rates: ReplayFeeRates,
}

impl ReplayInput {
    pub fn validate(&self) -> Result<(), String> {
        if self.prices.is_empty() {
            return Err("prices must not be empty".to_string());
        }
        for (index, price) in self.prices.iter().enumerate() {
            if !price.is_finite() || *price <= 0.0 {
                return Err(format!(
                    "prices[{index}] must be finite and positive, got {price}"
                ));
            }
        }
        if !self.initial_native_quantity.is_finite() {
            return Err(format!(
                "initial_native_quantity must be finite, got {}",
                self.initial_native_quantity
            ));
        }
        if self.leverage == 0 {
            return Err("leverage must be positive".to_string());
        }
        validate_config(&self.track_config)?;
        self.instrument_spec.validate()?;
        self.fee_rates.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayReport {
    pub samples: Vec<ReplaySample>,
    pub stats: ReplayStats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySample {
    pub price: f64,
    pub target_exposure: f64,
    pub position_native_quantity: f64,
    pub position_notional: f64,
    pub trade_native_quantity: f64,
    pub trade_notional: f64,
    pub estimated_fee: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayStats {
    pub trade_count: usize,
    pub trade_density: f64,
    pub max_abs_native_quantity: f64,
    pub max_abs_notional: f64,
    pub max_margin_requirement: f64,
    pub estimated_fee: f64,
    pub fee_asset: String,
    pub position_distribution: ReplayPositionDistribution,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayPositionDistribution {
    pub min_exposure: f64,
    pub max_exposure: f64,
    pub mean_abs_exposure: f64,
    pub min_native_quantity: f64,
    pub max_native_quantity: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayComparisonInput {
    pub baseline: ReplayInput,
    pub variants: Vec<ReplayParameterOverride>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayParameterOverride {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rebalance_units: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leverage: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notional_per_unit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower_price: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_price: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayComparisonReport {
    pub items: Vec<ReplayComparisonItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayComparisonItem {
    pub name: String,
    pub min_rebalance_units: f64,
    pub leverage: u32,
    pub notional_per_unit: f64,
    pub lower_price: f64,
    pub upper_price: f64,
    pub stats: ReplayStats,
}

pub fn compare_replay_parameters(
    input: &ReplayComparisonInput,
) -> Result<ReplayComparisonReport, String> {
    let mut items = Vec::with_capacity(input.variants.len() + 1);
    items.push(replay_comparison_item(
        "baseline".to_string(),
        &input.baseline,
    )?);

    for variant in &input.variants {
        if variant.name.trim().is_empty() {
            return Err("replay variant name must not be empty".to_string());
        }
        let mut replay_input = input.baseline.clone();
        if let Some(value) = variant.min_rebalance_units {
            replay_input.track_config.min_rebalance_units = value;
        }
        if let Some(value) = variant.leverage {
            replay_input.leverage = value;
        }
        if let Some(value) = variant.notional_per_unit {
            replay_input.track_config.notional_per_unit = value;
        }
        if let Some(value) = variant.lower_price {
            replay_input.track_config.lower_price = value;
        }
        if let Some(value) = variant.upper_price {
            replay_input.track_config.upper_price = value;
        }
        items.push(replay_comparison_item(variant.name.clone(), &replay_input)?);
    }

    Ok(ReplayComparisonReport { items })
}

fn replay_comparison_item(
    name: String,
    input: &ReplayInput,
) -> Result<ReplayComparisonItem, String> {
    let report = run_replay(input)?;
    Ok(ReplayComparisonItem {
        name,
        min_rebalance_units: input.track_config.min_rebalance_units,
        leverage: input.leverage,
        notional_per_unit: input.track_config.notional_per_unit,
        lower_price: input.track_config.lower_price,
        upper_price: input.track_config.upper_price,
        stats: report.stats,
    })
}

/// Runs the curve-immediate benchmark.
///
/// The result describes immediate curve tracking under a simplified taker-fill assumption. Use
/// the engine decision replay when manager, risk-acquisition, executor, or restart semantics are
/// part of the question.
pub fn run_replay(input: &ReplayInput) -> Result<ReplayReport, String> {
    input.validate()?;
    let native_quantity_per_unit = input
        .instrument_spec
        .position_qty_for_notional(
            Amount::new(
                input.track_config.notional_per_unit,
                input.instrument_spec.notional_asset().clone(),
            )?,
            input.track_config.band_center(),
        )?
        .value();
    if !native_quantity_per_unit.is_finite() || native_quantity_per_unit <= 0.0 {
        return Err("native quantity per exposure unit must be positive".to_string());
    }

    let mut position_native_quantity = input.initial_native_quantity;
    let mut current_exposure = position_native_quantity / native_quantity_per_unit;
    let mut samples = Vec::with_capacity(input.prices.len());
    let mut trade_count = 0;
    let mut estimated_fee = 0.0;
    let mut max_abs_native_quantity = position_native_quantity.abs();
    let mut max_abs_notional = position_notional(
        &input.instrument_spec,
        position_native_quantity,
        input.track_config.band_center(),
    )?;
    let mut min_exposure = current_exposure;
    let mut max_exposure = current_exposure;
    let mut min_native_quantity = position_native_quantity;
    let mut max_native_quantity = position_native_quantity;
    let mut abs_exposure_sum = 0.0;

    for price in &input.prices {
        let target_exposure = desired_exposure(*price, &input.track_config).0;
        let exposure_delta = target_exposure - current_exposure;
        let mut trade_native_quantity = 0.0;
        let mut trade_notional = 0.0;
        let mut sample_fee = 0.0;

        if exposure_delta.abs() + f64::EPSILON >= input.track_config.min_rebalance_units {
            let target_native_quantity = target_exposure * native_quantity_per_unit;
            trade_native_quantity = target_native_quantity - position_native_quantity;
            trade_notional =
                position_notional(&input.instrument_spec, trade_native_quantity, *price)?;
            sample_fee = estimate_settlement_fee(
                &input.instrument_spec,
                trade_notional,
                *price,
                input.fee_rates.taker_fee_rate,
            )?;
            estimated_fee += sample_fee;
            trade_count += 1;
            position_native_quantity = target_native_quantity;
            current_exposure = target_exposure;
        }

        let position_notional =
            position_notional(&input.instrument_spec, position_native_quantity, *price)?;
        max_abs_native_quantity = max_abs_native_quantity.max(position_native_quantity.abs());
        max_abs_notional = max_abs_notional.max(position_notional);
        min_exposure = min_exposure.min(current_exposure);
        max_exposure = max_exposure.max(current_exposure);
        min_native_quantity = min_native_quantity.min(position_native_quantity);
        max_native_quantity = max_native_quantity.max(position_native_quantity);
        abs_exposure_sum += current_exposure.abs();

        samples.push(ReplaySample {
            price: *price,
            target_exposure,
            position_native_quantity,
            position_notional,
            trade_native_quantity,
            trade_notional,
            estimated_fee: sample_fee,
        });
    }

    Ok(ReplayReport {
        stats: ReplayStats {
            trade_count,
            trade_density: trade_count as f64 / input.prices.len() as f64,
            max_abs_native_quantity,
            max_abs_notional,
            max_margin_requirement: max_abs_notional / input.leverage as f64,
            estimated_fee,
            fee_asset: input
                .instrument_spec
                .settlement_asset()
                .as_str()
                .to_string(),
            position_distribution: ReplayPositionDistribution {
                min_exposure,
                max_exposure,
                mean_abs_exposure: abs_exposure_sum / input.prices.len() as f64,
                min_native_quantity,
                max_native_quantity,
            },
        },
        samples,
    })
}

/// Converts a fee charged on trade notional into the exchange settlement asset.
///
/// Base-asset contracts settle the notional fee directly. Inverse contracts settle in the base
/// asset, so the notional fee is divided by the fill price.
pub fn estimate_settlement_fee(
    instrument_spec: &InstrumentSpec,
    trade_notional: f64,
    price: f64,
    fee_rate: f64,
) -> Result<f64, String> {
    if !trade_notional.is_finite() || trade_notional < 0.0 {
        return Err(format!(
            "trade_notional must be finite and non-negative, got {trade_notional}"
        ));
    }
    if !price.is_finite() || price <= 0.0 {
        return Err(format!(
            "fee price must be finite and positive, got {price}"
        ));
    }
    if !fee_rate.is_finite() || fee_rate < 0.0 {
        return Err(format!(
            "fee_rate must be finite and non-negative, got {fee_rate}"
        ));
    }

    let fee_notional = trade_notional * fee_rate;
    if !fee_notional.is_finite() {
        return Err("settlement fee notional must be finite".to_string());
    }
    let settlement_fee = if instrument_spec.notional_asset() == instrument_spec.settlement_asset() {
        fee_notional
    } else {
        match instrument_spec.position_basis() {
            PositionBasis::Contracts { .. }
                if instrument_spec.settlement_asset() == instrument_spec.base_asset() =>
            {
                fee_notional / price
            }
            _ => {
                return Err(format!(
                    "fee estimate cannot convert notional asset `{}` to settlement asset `{}` without an explicit valuation",
                    instrument_spec.notional_asset().as_str(),
                    instrument_spec.settlement_asset().as_str()
                ));
            }
        }
    };
    if !settlement_fee.is_finite() {
        return Err("settlement fee must be finite".to_string());
    }
    Ok(settlement_fee)
}

fn position_notional(
    instrument_spec: &InstrumentSpec,
    quantity: f64,
    price: f64,
) -> Result<f64, String> {
    instrument_spec
        .notional_from_position_qty(PositionQty::new(quantity)?, price)
        .map(|amount| amount.value())
}

fn validate_fee_rate(field: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value < 0.0 {
        return Err(format!(
            "{field} must be finite and non-negative, got {value}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::instrument::{Amount, Asset, InstrumentSpec};
    use crate::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use crate::track::{Instrument, Venue};

    use super::{ReplayFeeRates, ReplayInput, estimate_settlement_fee, run_replay};

    #[test]
    fn settlement_fee_keeps_base_asset_fee_in_settlement_notional() {
        let fee = estimate_settlement_fee(&base_spec(), 1_500.0, 65_000.0, 0.0005).unwrap();

        assert_close(fee, 0.75);
    }

    #[test]
    fn settlement_fee_converts_inverse_notional_to_settlement_asset() {
        let fee = estimate_settlement_fee(&inverse_spec(), 1_500.0, 65_000.0, 0.0005).unwrap();

        assert_close(fee, 0.000011538461538461538);
    }

    #[test]
    fn settlement_fee_rejects_invalid_notional_price_and_rate() {
        for (trade_notional, price, fee_rate, expected_field) in [
            (f64::NAN, 65_000.0, 0.0005, "trade_notional"),
            (-1.0, 65_000.0, 0.0005, "trade_notional"),
            (1_500.0, f64::INFINITY, 0.0005, "price"),
            (1_500.0, 0.0, 0.0005, "price"),
            (1_500.0, 65_000.0, f64::NAN, "fee_rate"),
            (1_500.0, 65_000.0, -0.0005, "fee_rate"),
        ] {
            let error = estimate_settlement_fee(&inverse_spec(), trade_notional, price, fee_rate)
                .unwrap_err();

            assert!(
                error.contains(expected_field),
                "expected fee error to mention {expected_field}, got {error}"
            );
        }
    }

    #[test]
    fn replay_input_deserializes_price_sequence_initial_position_fees_and_track_config() {
        let input: ReplayInput = serde_json::from_str(
            r#"
{
  "prices": [60000.0, 65000.0, 70000.0],
  "initial_native_quantity": 12.0,
  "leverage": 3,
  "track_config": {
    "lower_price": 60000.0,
    "upper_price": 70000.0,
    "long_exposure_units": 4.0,
    "short_exposure_units": 6.0,
    "notional_per_unit": 300.0,
    "min_rebalance_units": 0.25,
    "shape_family": "linear",
    "out_of_band_policy": "freeze",
    "risk_acquisition": {
      "initial_ratio": 0.5,
      "advantage_steps": 2.0,
      "min_release_steps": 1.0,
      "max_release_steps": 4.0,
      "catchup_ratio": 0.25,
      "stale_release_minutes": 60.0
    }
  },
  "instrument_spec": {
    "instrument": {"venue": "okx", "symbol": "BTC-USD-SWAP"},
    "base_asset": "BTC",
    "quote_asset": "USD",
    "notional_asset": "USD",
    "settlement_asset": "BTC",
    "position_basis": {
      "kind": "contracts",
      "contract_value": {"value": 100.0, "asset": "USD"}
    },
    "price_tick": 0.1,
    "price_precision": {"kind": "fixed_tick"},
    "position_qty_step": 1.0,
    "min_position_qty": 1.0,
    "min_notional": {"value": 0.0, "asset": "USD"}
  },
  "fee_rates": {
    "maker_fee_rate": 0.0002,
    "taker_fee_rate": 0.0005
  }
}
"#,
        )
        .unwrap();

        assert_eq!(input.prices, vec![60_000.0, 65_000.0, 70_000.0]);
        assert_eq!(input.initial_native_quantity, 12.0);
        assert_eq!(input.leverage, 3);
        assert_eq!(input.track_config.notional_per_unit, 300.0);
        assert_eq!(input.fee_rates.taker_fee_rate, 0.0005);
        assert_eq!(
            input.instrument_spec.instrument(),
            &Instrument::new(Venue::Okx, "BTC-USD-SWAP")
        );
        input.validate().unwrap();
    }

    #[test]
    fn replay_input_rejects_empty_or_invalid_prices() {
        let mut input = replay_input();
        input.prices.clear();
        assert!(input.validate().unwrap_err().contains("prices"));

        input.prices = vec![60_000.0, 0.0];
        let error = input.validate().unwrap_err();
        assert!(error.contains("prices[1]"));
        assert!(error.contains("positive"));
    }

    #[test]
    fn replay_outputs_inverse_position_and_trade_statistics() {
        let report = run_replay(&replay_input()).unwrap();

        assert_eq!(report.samples.len(), 3);
        assert_eq!(report.stats.trade_count, 2);
        assert_close(report.stats.trade_density, 0.6666666666666666);
        assert_eq!(report.stats.max_abs_native_quantity, 18.0);
        assert_eq!(report.stats.max_abs_notional, 1_800.0);
        assert_eq!(report.stats.max_margin_requirement, 600.0);
        assert_eq!(report.stats.fee_asset, "BTC");
        assert_close(report.stats.estimated_fee, 0.00002225274725274725);
        assert_eq!(report.stats.position_distribution.min_exposure, -6.0);
        assert_eq!(report.stats.position_distribution.max_exposure, 4.0);
        assert_close(
            report.stats.position_distribution.mean_abs_exposure,
            3.6666666666666665,
        );
        assert_eq!(report.samples[0].target_exposure, 4.0);
        assert_eq!(report.samples[0].position_native_quantity, 12.0);
        assert_eq!(report.samples[0].trade_native_quantity, 0.0);
        assert_eq!(report.samples[1].target_exposure, -1.0);
        assert_eq!(report.samples[1].position_native_quantity, -3.0);
        assert_eq!(report.samples[2].target_exposure, -6.0);
        assert_eq!(report.samples[2].position_native_quantity, -18.0);
    }

    #[test]
    fn replay_compares_parameter_overrides() {
        let report = super::compare_replay_parameters(&super::ReplayComparisonInput {
            baseline: replay_input(),
            variants: vec![
                super::ReplayParameterOverride {
                    name: "high_min_rebalance".to_string(),
                    min_rebalance_units: Some(6.0),
                    leverage: None,
                    notional_per_unit: None,
                    lower_price: None,
                    upper_price: None,
                },
                super::ReplayParameterOverride {
                    name: "higher_leverage".to_string(),
                    min_rebalance_units: None,
                    leverage: Some(6),
                    notional_per_unit: None,
                    lower_price: None,
                    upper_price: None,
                },
                super::ReplayParameterOverride {
                    name: "larger_unit".to_string(),
                    min_rebalance_units: None,
                    leverage: None,
                    notional_per_unit: Some(500.0),
                    lower_price: None,
                    upper_price: None,
                },
                super::ReplayParameterOverride {
                    name: "wider_band".to_string(),
                    min_rebalance_units: None,
                    leverage: None,
                    notional_per_unit: None,
                    lower_price: Some(55_000.0),
                    upper_price: Some(75_000.0),
                },
            ],
        })
        .unwrap();

        let baseline = item(&report, "baseline");
        let high_min_rebalance = item(&report, "high_min_rebalance");
        let higher_leverage = item(&report, "higher_leverage");
        let larger_unit = item(&report, "larger_unit");
        let wider_band = item(&report, "wider_band");

        assert_eq!(report.items.len(), 5);
        assert!(high_min_rebalance.stats.trade_count < baseline.stats.trade_count);
        assert!(
            higher_leverage.stats.max_margin_requirement < baseline.stats.max_margin_requirement
        );
        assert!(larger_unit.stats.max_abs_notional > baseline.stats.max_abs_notional);
        assert!(wider_band.stats.max_abs_native_quantity < baseline.stats.max_abs_native_quantity);
    }

    #[test]
    fn replay_fixed_sample_output_is_stable() {
        let first = run_replay(&replay_input()).unwrap();
        let second = run_replay(&replay_input()).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::json!({
                "samples": [
                    {
                        "price": 60000.0,
                        "target_exposure": 4.0,
                        "position_native_quantity": 12.0,
                        "position_notional": 1200.0,
                        "trade_native_quantity": 0.0,
                        "trade_notional": 0.0,
                        "estimated_fee": 0.0
                    },
                    {
                        "price": 65000.0,
                        "target_exposure": -1.0,
                        "position_native_quantity": -3.0,
                        "position_notional": 300.0,
                        "trade_native_quantity": -15.0,
                        "trade_notional": 1500.0,
                        "estimated_fee": 0.000011538461538461538
                    },
                    {
                        "price": 70000.0,
                        "target_exposure": -6.0,
                        "position_native_quantity": -18.0,
                        "position_notional": 1800.0,
                        "trade_native_quantity": -15.0,
                        "trade_notional": 1500.0,
                        "estimated_fee": 0.000010714285714285714
                    }
                ],
                "stats": {
                    "trade_count": 2,
                    "trade_density": 0.6666666666666666,
                    "max_abs_native_quantity": 18.0,
                    "max_abs_notional": 1800.0,
                    "max_margin_requirement": 600.0,
                    "estimated_fee": 0.000022252747252747252,
                    "fee_asset": "BTC",
                    "position_distribution": {
                        "min_exposure": -6.0,
                        "max_exposure": 4.0,
                        "mean_abs_exposure": 3.6666666666666665,
                        "min_native_quantity": -18.0,
                        "max_native_quantity": 12.0
                    }
                }
            })
        );
    }

    #[test]
    fn inverse_contract_exposure_unit_does_not_drift_with_price() {
        let input = replay_input();
        let quantities = input
            .prices
            .iter()
            .map(|price| {
                input
                    .instrument_spec
                    .position_qty_for_notional(
                        Amount::new(
                            input.track_config.notional_per_unit,
                            input.instrument_spec.notional_asset().clone(),
                        )
                        .unwrap(),
                        *price,
                    )
                    .unwrap()
                    .value()
            })
            .collect::<Vec<_>>();

        assert_eq!(quantities, vec![3.0, 3.0, 3.0]);
    }

    fn replay_input() -> ReplayInput {
        ReplayInput {
            prices: vec![60_000.0, 65_000.0, 70_000.0],
            initial_native_quantity: 12.0,
            leverage: 3,
            track_config: TrackConfig {
                lower_price: 60_000.0,
                upper_price: 70_000.0,
                long_exposure_units: 4.0,
                short_exposure_units: 6.0,
                notional_per_unit: 300.0,
                min_rebalance_units: 0.25,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            instrument_spec: inverse_spec(),
            fee_rates: ReplayFeeRates {
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
            },
        }
    }

    fn inverse_spec() -> InstrumentSpec {
        let usd = Asset::try_new("USD").unwrap();
        InstrumentSpec::try_inverse(
            Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            Asset::try_new("BTC").unwrap(),
            usd.clone(),
            Asset::try_new("BTC").unwrap(),
            Amount::new(100.0, usd.clone()).unwrap(),
            Default::default(),
            0.1,
            1.0,
            1.0,
            Amount::new(0.0, usd).unwrap(),
        )
        .unwrap()
    }

    fn base_spec() -> InstrumentSpec {
        let usdt = Asset::try_new("USDT").unwrap();
        InstrumentSpec::try_linear(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            Asset::try_new("BTC").unwrap(),
            usdt.clone(),
            usdt.clone(),
            Default::default(),
            0.1,
            0.001,
            0.001,
            Amount::new(5.0, usdt).unwrap(),
        )
        .unwrap()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {actual} to be close to {expected}"
        );
    }

    fn item<'a>(
        report: &'a super::ReplayComparisonReport,
        name: &str,
    ) -> &'a super::ReplayComparisonItem {
        report
            .items
            .iter()
            .find(|item| item.name == name)
            .unwrap_or_else(|| panic!("missing comparison item {name}"))
    }
}
