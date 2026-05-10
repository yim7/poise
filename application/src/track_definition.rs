use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use poise_core::track::{TrackDefinition, TrackId};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackDefinitionRegistry {
    tracks: BTreeMap<String, TrackDefinition>,
}

impl TrackDefinitionRegistry {
    pub fn new(definitions: Vec<TrackDefinition>) -> Result<Self> {
        let mut tracks = BTreeMap::new();
        for definition in definitions {
            let track_id = definition.track_id().as_str().to_string();
            if tracks.insert(track_id.clone(), definition).is_some() {
                return Err(anyhow!("duplicate track id `{track_id}`"));
            }
        }
        Ok(Self { tracks })
    }

    pub fn get(&self, track_id: &TrackId) -> Option<&TrackDefinition> {
        self.tracks.get(track_id.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TrackDefinition> {
        self.tracks.values()
    }
}

#[cfg(test)]
mod tests {
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};

    use super::TrackDefinitionRegistry;

    fn track_definition(track_id: &str) -> TrackDefinition {
        TrackDefinition::try_new(
            TrackId::new(track_id),
            Instrument::new(Venue::Binance, "BTCUSDT"),
            TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 6.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_increase_delay: None,
            },
            Some(3_000.0),
            LossLimits {
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
            },
            Some(30),
        )
        .unwrap()
    }

    #[test]
    fn track_definition_registry_returns_definition_by_track_id() {
        let registry = TrackDefinitionRegistry::new(vec![track_definition("btc-core")]).unwrap();
        let definition = registry.get(&TrackId::new("btc-core")).unwrap();

        assert_eq!(definition.track_id().as_str(), "btc-core");
        assert_eq!(definition.instrument().symbol, "BTCUSDT");
        assert_eq!(definition.max_notional(), 3_000.0);
    }

    #[test]
    fn track_definition_registry_rejects_duplicate_track_id() {
        let error = TrackDefinitionRegistry::new(vec![
            track_definition("btc-core"),
            track_definition("btc-core"),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("duplicate track id `btc-core`"));
    }
}
