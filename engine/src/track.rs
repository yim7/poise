use poise_core::risk::LossLimits;
use poise_core::strategy::TrackConfig;
use poise_core::track::{Instrument, TrackId};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackDefinition {
    pub id: TrackId,
    pub instrument: Instrument,
    pub config: TrackConfig,
    pub max_notional: f64,
    pub loss_limits: LossLimits,
}

impl TrackDefinition {
    pub fn new(
        id: TrackId,
        instrument: Instrument,
        config: TrackConfig,
        max_notional: f64,
        loss_limits: LossLimits,
    ) -> Self {
        Self {
            id,
            instrument,
            config,
            max_notional,
            loss_limits,
        }
    }
}
