use poise_core::risk::CapacityBudget;
use poise_core::strategy::TrackConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRestoreRevision(String);

impl TrackRestoreRevision {
    pub fn for_track(instrument: &Instrument, track_config: &TrackConfig) -> Self {
        let payload = serde_json::json!({
            "instrument": instrument,
            "track_config": track_config,
        });
        let mut hasher = Sha256::new();
        hasher.update(payload.to_string().as_bytes());
        Self(format!("{:x}", hasher.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRuntimeSeed {
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub track_config: TrackConfig,
    pub budget: CapacityBudget,
    pub tick_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostRestoreConstraints {
    pub budget: CapacityBudget,
    pub tick_timeout_secs: u64,
}

#[cfg(test)]
mod tests {
    use poise_core::risk::CapacityBudget;
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};

    use crate::track::{Instrument, Venue};

    use super::{PostRestoreConstraints, TrackRestoreRevision};

    #[test]
    fn track_restore_revision_is_stable_for_same_instrument_and_track_config() {
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let track_config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        };

        let left = TrackRestoreRevision::for_track(&instrument, &track_config);
        let right = TrackRestoreRevision::for_track(&instrument, &track_config);

        assert_eq!(left, right);
    }

    #[test]
    fn track_restore_revision_ignores_budget_and_tick_timeout_changes() {
        let instrument = Instrument::new(Venue::Binance, "BTCUSDT");
        let track_config = TrackConfig {
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Linear,
            out_of_band_policy: OutOfBandPolicy::Freeze,
        };

        let revision = TrackRestoreRevision::for_track(&instrument, &track_config);
        let left = PostRestoreConstraints {
            budget: CapacityBudget {
                max_notional: 3000.0,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
            },
            tick_timeout_secs: 30,
        };
        let right = PostRestoreConstraints {
            budget: CapacityBudget {
                max_notional: 4200.0,
                daily_loss_limit: 200.0,
                total_loss_limit: 800.0,
            },
            tick_timeout_secs: 45,
        };

        assert_eq!(revision, TrackRestoreRevision::for_track(&instrument, &track_config));
        assert_ne!(left, right);
    }
}
