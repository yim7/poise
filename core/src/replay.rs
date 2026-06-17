use serde::{Deserialize, Serialize};

use crate::strategy::{TrackConfig, desired_exposure, validate_config};
use crate::types::{ExchangeRules, QuantityKind};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayInput {
    pub prices: Vec<f64>,
    pub initial_native_quantity: f64,
    pub track_config: TrackConfig,
    pub exchange_rules: ExchangeRules,
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
        validate_config(&self.track_config)?;
        validate_exchange_rules(&self.exchange_rules)?;
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

pub fn run_replay(input: &ReplayInput) -> Result<ReplayReport, String> {
    input.validate()?;
    let native_quantity_per_unit = input.exchange_rules.native_qty_per_exposure_unit(
        input.track_config.notional_per_unit,
        input.track_config.band_center(),
    );
    if !native_quantity_per_unit.is_finite() || native_quantity_per_unit <= 0.0 {
        return Err("native quantity per exposure unit must be positive".to_string());
    }

    let mut position_native_quantity = input.initial_native_quantity;
    let mut current_exposure = position_native_quantity / native_quantity_per_unit;
    let mut samples = Vec::with_capacity(input.prices.len());
    let mut trade_count = 0;
    let mut estimated_fee = 0.0;
    let mut max_abs_native_quantity = position_native_quantity.abs();
    let mut max_abs_notional = input
        .exchange_rules
        .notional_from_native_qty(position_native_quantity, input.track_config.band_center());
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
            trade_notional = input
                .exchange_rules
                .notional_from_native_qty(trade_native_quantity, *price);
            sample_fee = estimate_fee(&input.exchange_rules, trade_notional, *price)?;
            estimated_fee += sample_fee;
            trade_count += 1;
            position_native_quantity = target_native_quantity;
            current_exposure = target_exposure;
        }

        let position_notional = input
            .exchange_rules
            .notional_from_native_qty(position_native_quantity, *price);
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
            estimated_fee,
            fee_asset: input.exchange_rules.settlement_asset.clone(),
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

fn estimate_fee(rules: &ExchangeRules, trade_notional: f64, price: f64) -> Result<f64, String> {
    let fee_notional = trade_notional * rules.taker_fee_rate;
    match rules.quantity_kind {
        QuantityKind::BaseAsset => Ok(fee_notional),
        QuantityKind::InverseContract => {
            if !price.is_finite() || price <= 0.0 {
                return Err(format!(
                    "fee price must be finite and positive, got {price}"
                ));
            }
            Ok(fee_notional / price)
        }
    }
}

fn validate_exchange_rules(rules: &ExchangeRules) -> Result<(), String> {
    if !rules.price_tick.is_finite() || rules.price_tick <= 0.0 {
        return Err(format!(
            "price_tick must be finite and positive, got {}",
            rules.price_tick
        ));
    }
    if !rules.quantity_step.is_finite() || rules.quantity_step <= 0.0 {
        return Err(format!(
            "quantity_step must be finite and positive, got {}",
            rules.quantity_step
        ));
    }
    if !rules.min_qty.is_finite() || rules.min_qty < 0.0 {
        return Err(format!(
            "min_qty must be finite and non-negative, got {}",
            rules.min_qty
        ));
    }
    if !rules.min_notional.is_finite() || rules.min_notional < 0.0 {
        return Err(format!(
            "min_notional must be finite and non-negative, got {}",
            rules.min_notional
        ));
    }
    if rules.settlement_asset.trim().is_empty() {
        return Err("settlement_asset must not be empty".to_string());
    }
    if !rules.maker_fee_rate.is_finite() || rules.maker_fee_rate < 0.0 {
        return Err(format!(
            "maker_fee_rate must be finite and non-negative, got {}",
            rules.maker_fee_rate
        ));
    }
    if !rules.taker_fee_rate.is_finite() || rules.taker_fee_rate < 0.0 {
        return Err(format!(
            "taker_fee_rate must be finite and non-negative, got {}",
            rules.taker_fee_rate
        ));
    }
    if matches!(rules.quantity_kind, QuantityKind::InverseContract) {
        let Some(contract_notional) = rules.contract_notional else {
            return Err("inverse replay input requires contract_notional".to_string());
        };
        if !contract_notional.is_finite() || contract_notional <= 0.0 {
            return Err(format!(
                "contract_notional must be finite and positive, got {contract_notional}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use crate::types::{ExchangeRules, QuantityKind};

    use super::{ReplayInput, run_replay};

    #[test]
    fn replay_input_deserializes_price_sequence_initial_position_fees_and_track_config() {
        let input: ReplayInput = serde_json::from_str(
            r#"
{
  "prices": [60000.0, 65000.0, 70000.0],
  "initial_native_quantity": 12.0,
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
  "exchange_rules": {
    "price_tick": 0.1,
    "price_precision": {"kind": "fixed_tick"},
    "quantity_kind": "inverse_contract",
    "contract_notional": 100.0,
    "settlement_asset": "BTC",
    "quantity_step": 1.0,
    "min_qty": 1.0,
    "min_notional": 0.0,
    "maker_fee_rate": 0.0002,
    "taker_fee_rate": 0.0005
  }
}
"#,
        )
        .unwrap();

        assert_eq!(input.prices, vec![60_000.0, 65_000.0, 70_000.0]);
        assert_eq!(input.initial_native_quantity, 12.0);
        assert_eq!(input.track_config.notional_per_unit, 300.0);
        assert_eq!(input.exchange_rules.taker_fee_rate, 0.0005);
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

    fn replay_input() -> ReplayInput {
        ReplayInput {
            prices: vec![60_000.0, 65_000.0, 70_000.0],
            initial_native_quantity: 12.0,
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
            exchange_rules: inverse_rules(),
        }
    }

    fn inverse_rules() -> ExchangeRules {
        ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::InverseContract,
            contract_notional: Some(100.0),
            settlement_asset: "BTC".to_string(),
            quantity_step: 1.0,
            min_qty: 1.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {actual} to be close to {expected}"
        );
    }
}
