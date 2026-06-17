use serde::{Deserialize, Serialize};

use crate::strategy::{TrackConfig, validate_config};
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

    use super::ReplayInput;

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
}
