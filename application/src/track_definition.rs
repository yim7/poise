use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use poise_core::risk::{CapacityBudget, validate_capacity_budget};
use poise_core::strategy::{
    DEFAULT_MIN_REBALANCE_UNITS, OutOfBandPolicy, ShapeFamily, TrackConfig, validate_config,
};
use poise_engine::persisted_runtime::{
    PostRestoreConstraints, TrackRestoreRevision, TrackRuntimeSeed,
};
use poise_engine::track::{Instrument, TrackId, Venue};

const DEFAULT_TICK_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredTrackInput {
    pub track_id: TrackId,
    pub venue: Venue,
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_exposure_units: f64,
    pub short_exposure_units: f64,
    pub notional_per_unit: f64,
    pub min_rebalance_units: Option<f64>,
    pub shape_family: Option<ShapeFamily>,
    pub out_of_band_policy: Option<OutOfBandPolicy>,
    pub max_notional: Option<f64>,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
    pub tick_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredTrackDefinition {
    track_id: TrackId,
    instrument: Instrument,
    track_config: TrackConfig,
    budget: CapacityBudget,
    tick_timeout_secs: u64,
}

impl ConfiguredTrackDefinition {
    pub fn try_from_input(input: ConfiguredTrackInput) -> Result<Self> {
        let track_config = TrackConfig {
            lower_price: input.lower_price,
            upper_price: input.upper_price,
            long_exposure_units: input.long_exposure_units,
            short_exposure_units: input.short_exposure_units,
            notional_per_unit: input.notional_per_unit,
            min_rebalance_units: input
                .min_rebalance_units
                .unwrap_or(DEFAULT_MIN_REBALANCE_UNITS),
            shape_family: input.shape_family.unwrap_or(ShapeFamily::Linear),
            out_of_band_policy: input.out_of_band_policy.unwrap_or(OutOfBandPolicy::Freeze),
        };
        validate_config(&track_config).map_err(|error| anyhow!(error))?;

        let implied_max_notional = track_config
            .long_exposure_units
            .max(track_config.short_exposure_units)
            * track_config.notional_per_unit;
        let budget = CapacityBudget {
            max_notional: input.max_notional.unwrap_or(implied_max_notional),
            daily_loss_limit: input.daily_loss_limit,
            total_loss_limit: input.total_loss_limit,
        };
        validate_capacity_budget(&budget).map_err(|error| anyhow!(error))?;

        Ok(Self {
            track_id: input.track_id,
            instrument: Instrument::new(input.venue, input.symbol),
            track_config,
            budget,
            tick_timeout_secs: input.tick_timeout_secs.unwrap_or(DEFAULT_TICK_TIMEOUT_SECS),
        })
    }

    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn track_config(&self) -> TrackConfig {
        self.track_config.clone()
    }

    pub fn budget(&self) -> CapacityBudget {
        self.budget.clone()
    }

    pub fn tick_timeout_secs(&self) -> u64 {
        self.tick_timeout_secs
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackPreparedDefinition {
    track_id: TrackId,
    instrument: Instrument,
    track_config: TrackConfig,
    budget: CapacityBudget,
    tick_timeout_secs: u64,
    restore_revision: TrackRestoreRevision,
}

impl TrackPreparedDefinition {
    pub fn from_configured(definition: ConfiguredTrackDefinition) -> Self {
        let restore_revision =
            TrackRestoreRevision::for_track(definition.instrument(), &definition.track_config);
        Self {
            track_id: definition.track_id,
            instrument: definition.instrument,
            track_config: definition.track_config,
            budget: definition.budget,
            tick_timeout_secs: definition.tick_timeout_secs,
            restore_revision,
        }
    }

    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn track_config(&self) -> &TrackConfig {
        &self.track_config
    }

    pub fn budget(&self) -> CapacityBudget {
        self.budget.clone()
    }

    pub fn tick_timeout_secs(&self) -> u64 {
        self.tick_timeout_secs
    }

    pub fn restore_revision(&self) -> &TrackRestoreRevision {
        &self.restore_revision
    }

    pub fn read_definition(&self) -> TrackReadDefinition {
        TrackReadDefinition {
            track_id: self.track_id.clone(),
            instrument: self.instrument.clone(),
            track_config: self.track_config.clone(),
            budget: self.budget.clone(),
        }
    }

    pub fn runtime_seed(&self) -> TrackRuntimeSeed {
        TrackRuntimeSeed {
            track_id: self.track_id.clone(),
            instrument: self.instrument.clone(),
            track_config: self.track_config.clone(),
            budget: self.budget.clone(),
            tick_timeout_secs: self.tick_timeout_secs,
        }
    }

    pub fn post_restore_constraints(&self) -> PostRestoreConstraints {
        PostRestoreConstraints {
            budget: self.budget.clone(),
            tick_timeout_secs: self.tick_timeout_secs,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadDefinition {
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub track_config: TrackConfig,
    pub budget: CapacityBudget,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreparedTrackRegistry {
    tracks: BTreeMap<String, TrackPreparedDefinition>,
}

impl PreparedTrackRegistry {
    pub fn new(definitions: Vec<ConfiguredTrackDefinition>) -> Result<Self> {
        let mut tracks = BTreeMap::new();
        for definition in definitions {
            let prepared = TrackPreparedDefinition::from_configured(definition);
            let track_id = prepared.track_id().as_str().to_string();
            if tracks.insert(track_id.clone(), prepared).is_some() {
                return Err(anyhow!("duplicate track id `{track_id}`"));
            }
        }
        Ok(Self { tracks })
    }

    pub fn get(&self, track_id: &TrackId) -> Option<&TrackPreparedDefinition> {
        self.tracks.get(track_id.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TrackPreparedDefinition> {
        self.tracks.values()
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{OutOfBandPolicy, ShapeFamily};
    use poise_engine::track::{TrackId, Venue};

    use super::{ConfiguredTrackDefinition, ConfiguredTrackInput, PreparedTrackRegistry};

    #[test]
    fn configured_track_definition_expands_defaults_and_validates_budget() {
        let definition = ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
            track_id: TrackId::new("btc-core"),
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: None,
            shape_family: None,
            out_of_band_policy: None,
            max_notional: None,
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
        })
        .unwrap();

        assert!((definition.track_config().min_rebalance_units - 0.5).abs() < f64::EPSILON);
        assert_eq!(definition.track_config().shape_family, ShapeFamily::Linear);
        assert_eq!(
            definition.track_config().out_of_band_policy,
            OutOfBandPolicy::Freeze
        );
        assert_eq!(definition.budget().max_notional, 3000.0);
        assert_eq!(definition.tick_timeout_secs(), 30);
    }

    #[test]
    fn prepared_track_registry_projects_read_definition_runtime_seed_and_constraints() {
        let configured = ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
            track_id: TrackId::new("btc-core"),
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 6.0,
            notional_per_unit: 375.0,
            min_rebalance_units: Some(0.75),
            shape_family: Some(ShapeFamily::Inertial),
            out_of_band_policy: Some(OutOfBandPolicy::Hold),
            max_notional: Some(4200.0),
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: Some(45),
        })
        .unwrap();
        let registry = PreparedTrackRegistry::new(vec![configured]).unwrap();
        let prepared = registry.get(&TrackId::new("btc-core")).unwrap();

        let read_definition = prepared.read_definition();
        assert_eq!(read_definition.track_id.as_str(), "btc-core");
        assert_eq!(read_definition.instrument.symbol, "BTCUSDT");
        assert_eq!(read_definition.budget.max_notional, 4200.0);

        let runtime_seed = prepared.runtime_seed();
        assert_eq!(runtime_seed.track_id.as_str(), "btc-core");
        assert_eq!(runtime_seed.instrument.symbol, "BTCUSDT");

        let constraints = prepared.post_restore_constraints();
        assert_eq!(constraints.budget.max_notional, 4200.0);
        assert_eq!(constraints.tick_timeout_secs, 45);
    }
}
