use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use poise_core::risk::{LossLimits, validate_loss_limits, validate_max_notional};
use poise_core::strategy::{
    BandProtectionPolicy, DEFAULT_MIN_REBALANCE_UNITS, ShapeFamily, TrackConfig, validate_config,
};
use poise_core::types::Exposure;
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
    pub out_of_band_policy: Option<BandProtectionPolicy>,
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
    max_notional: f64,
    loss_limits: LossLimits,
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
            out_of_band_policy: input
                .out_of_band_policy
                .unwrap_or(BandProtectionPolicy::Freeze),
        };
        validate_config(&track_config).map_err(|error| anyhow!(error))?;

        let implied_max_notional = track_config
            .long_exposure_units
            .max(track_config.short_exposure_units)
            * track_config.notional_per_unit;
        let max_notional = input.max_notional.unwrap_or(implied_max_notional);
        let loss_limits = LossLimits {
            daily_loss_limit: input.daily_loss_limit,
            total_loss_limit: input.total_loss_limit,
        };
        validate_max_notional(max_notional).map_err(|error| anyhow!(error))?;
        validate_loss_limits(&loss_limits).map_err(|error| anyhow!(error))?;

        Ok(Self {
            track_id: input.track_id,
            instrument: Instrument::new(input.venue, input.symbol),
            track_config,
            max_notional,
            loss_limits,
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

    pub fn curve_max_notional(&self) -> f64 {
        curve_max_notional(&self.track_config)
    }

    pub fn max_notional(&self) -> f64 {
        self.max_notional
    }

    pub fn effective_max_notional(&self) -> f64 {
        effective_max_notional(&self.track_config, self.max_notional)
    }

    pub fn loss_limits(&self) -> &LossLimits {
        &self.loss_limits
    }

    pub fn tick_timeout_secs(&self) -> u64 {
        self.tick_timeout_secs
    }

    pub fn read_definition(&self) -> TrackReadDefinition {
        TrackReadDefinition {
            track_id: self.track_id.clone(),
            instrument: self.instrument.clone(),
            track_config: self.track_config.clone(),
            max_notional: self.max_notional,
            loss_limits: self.loss_limits.clone(),
        }
    }

    pub fn startup_definition(&self) -> TrackStartupDefinition {
        TrackStartupDefinition {
            track_id: self.track_id.clone(),
            instrument: self.instrument.clone(),
            track_config: self.track_config.clone(),
            max_notional: self.max_notional,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackReadDefinition {
    pub track_id: TrackId,
    pub instrument: Instrument,
    pub track_config: TrackConfig,
    pub max_notional: f64,
    pub loss_limits: LossLimits,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackStartupDefinition {
    track_id: TrackId,
    instrument: Instrument,
    track_config: TrackConfig,
    max_notional: f64,
}

impl TrackStartupDefinition {
    pub fn track_id(&self) -> &TrackId {
        &self.track_id
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn required_additional_notional(&self, position_qty: f64) -> f64 {
        let current_position_notional = self
            .track_config
            .abs_notional_from_position_qty(position_qty);
        (self.max_notional - current_position_notional).max(0.0)
    }

    pub fn exposure_from_position_qty(&self, position_qty: f64) -> Exposure {
        let unit_qty = self.track_config.base_qty_per_unit();
        if unit_qty <= f64::EPSILON {
            Exposure(0.0)
        } else {
            Exposure(position_qty / unit_qty)
        }
    }
}

pub fn curve_max_notional(config: &TrackConfig) -> f64 {
    config.long_exposure_units.max(config.short_exposure_units) * config.notional_per_unit
}

pub fn effective_max_notional(config: &TrackConfig, max_notional: f64) -> f64 {
    curve_max_notional(config).min(max_notional)
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreparedTrackRegistry {
    tracks: BTreeMap<String, ConfiguredTrackDefinition>,
}

impl PreparedTrackRegistry {
    pub fn new(definitions: Vec<ConfiguredTrackDefinition>) -> Result<Self> {
        let mut tracks = BTreeMap::new();
        for definition in definitions {
            let track_id = definition.track_id().as_str().to_string();
            if tracks.insert(track_id.clone(), definition).is_some() {
                return Err(anyhow!("duplicate track id `{track_id}`"));
            }
        }
        Ok(Self { tracks })
    }

    pub fn get(&self, track_id: &TrackId) -> Option<&ConfiguredTrackDefinition> {
        self.tracks.get(track_id.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = &ConfiguredTrackDefinition> {
        self.tracks.values()
    }
}

#[cfg(test)]
mod tests {
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily};
    use poise_engine::track::{TrackId, Venue};

    use super::{ConfiguredTrackDefinition, ConfiguredTrackInput, PreparedTrackRegistry};

    fn startup_definition_fixture(max_notional: Option<f64>) -> ConfiguredTrackDefinition {
        ConfiguredTrackDefinition::try_from_input(ConfiguredTrackInput {
            track_id: TrackId::new("btc-core"),
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            lower_price: 90.0,
            upper_price: 110.0,
            long_exposure_units: 8.0,
            short_exposure_units: 8.0,
            notional_per_unit: 375.0,
            min_rebalance_units: Some(0.5),
            shape_family: Some(ShapeFamily::Linear),
            out_of_band_policy: Some(BandProtectionPolicy::Freeze),
            max_notional,
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: Some(30),
        })
        .unwrap()
    }

    #[test]
    fn configured_track_definition_expands_defaults_and_validates_definition_limits() {
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
            BandProtectionPolicy::Freeze
        );
        assert_eq!(definition.max_notional(), 3000.0);
        assert_eq!(definition.loss_limits().daily_loss_limit, 300.0);
        assert_eq!(definition.loss_limits().total_loss_limit, 600.0);
        assert_eq!(definition.tick_timeout_secs(), 30);
    }

    #[test]
    fn configured_track_definition_derives_effective_max_notional_from_curve_and_config_limit() {
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
            max_notional: Some(2_000.0),
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: None,
        })
        .unwrap();

        assert_eq!(definition.curve_max_notional(), 3_000.0);
        assert_eq!(definition.max_notional(), 2_000.0);
        assert_eq!(definition.effective_max_notional(), 2_000.0);
    }

    #[test]
    fn prepared_track_registry_returns_normalized_definition_with_read_projection() {
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
            out_of_band_policy: Some(BandProtectionPolicy::Terminate),
            max_notional: Some(4200.0),
            daily_loss_limit: 300.0,
            total_loss_limit: 600.0,
            tick_timeout_secs: Some(45),
        })
        .unwrap();
        let registry = PreparedTrackRegistry::new(vec![configured]).unwrap();
        let prepared: &ConfiguredTrackDefinition = registry.get(&TrackId::new("btc-core")).unwrap();

        let read_definition = prepared.read_definition();
        assert_eq!(read_definition.track_id.as_str(), "btc-core");
        assert_eq!(read_definition.instrument.symbol, "BTCUSDT");
        assert_eq!(read_definition.max_notional, 4200.0);
        assert_eq!(read_definition.loss_limits.daily_loss_limit, 300.0);
        assert_eq!(read_definition.loss_limits.total_loss_limit, 600.0);
        assert_eq!(prepared.tick_timeout_secs(), 45);
        assert_eq!(prepared.effective_max_notional(), 3_000.0);
    }

    #[test]
    fn configured_track_definition_projects_startup_definition() {
        let prepared = startup_definition_fixture(Some(3_000.0));
        let startup = prepared.startup_definition();

        assert_eq!(startup.track_id().as_str(), "btc-core");
        assert_eq!(startup.instrument().symbol, "BTCUSDT");
    }

    #[test]
    fn startup_definition_required_additional_notional_subtracts_existing_position_notional() {
        let prepared = startup_definition_fixture(Some(3_000.0));
        let startup = prepared.startup_definition();

        // center = 100, notional_per_unit = 375, 所以 1 unit = 3.75 qty。
        // 现有 4 units -> qty = 15.0，对应已占用 1_500 notional。
        assert_eq!(startup.required_additional_notional(15.0), 1_500.0);
    }

    #[test]
    fn startup_definition_required_additional_notional_clamps_to_zero() {
        let prepared = startup_definition_fixture(Some(3_000.0));
        let startup = prepared.startup_definition();

        // 8 units -> qty = 30.0，正好覆盖 3_000 notional。
        assert_eq!(startup.required_additional_notional(30.0), 0.0);
        assert_eq!(startup.required_additional_notional(45.0), 0.0);
    }
}
