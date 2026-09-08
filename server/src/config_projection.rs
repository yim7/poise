use poise_core::track::TrackDefinition;
use poise_protocol::{
    BandFlattenTrigger, BandProtectionPolicy, BandRecoverPolicy, RiskAcquisitionConfigView,
    ShapeFamily, TrackConfigurationView,
};

pub(crate) fn configuration_view_from_definition(
    definition: &TrackDefinition,
) -> TrackConfigurationView {
    let config = definition.track_config();
    TrackConfigurationView {
        lower_price: config.lower_price,
        upper_price: config.upper_price,
        long_exposure_units: config.long_exposure_units,
        short_exposure_units: config.short_exposure_units,
        notional_per_unit: config.notional_per_unit,
        min_rebalance_units: config.min_rebalance_units,
        shape_family: project_shape_family(config.shape_family),
        out_of_band_policy: project_out_of_band_policy(config.out_of_band_policy),
        max_notional: definition.max_notional(),
        daily_loss_limit: definition.loss_limits().daily_loss_limit,
        total_loss_limit: definition.loss_limits().total_loss_limit,
        risk_acquisition: project_risk_acquisition(config.risk_acquisition),
    }
}

pub(crate) fn project_shape_family(value: poise_core::strategy::ShapeFamily) -> ShapeFamily {
    match value {
        poise_core::strategy::ShapeFamily::Linear => ShapeFamily::Linear,
        poise_core::strategy::ShapeFamily::Inertial => ShapeFamily::Inertial,
        poise_core::strategy::ShapeFamily::Responsive => ShapeFamily::Responsive,
    }
}

pub(crate) fn project_out_of_band_policy(
    value: poise_core::strategy::BandProtectionPolicy,
) -> BandProtectionPolicy {
    match value {
        poise_core::strategy::BandProtectionPolicy::Freeze => BandProtectionPolicy::Freeze,
        poise_core::strategy::BandProtectionPolicy::Terminate => BandProtectionPolicy::Terminate,
        poise_core::strategy::BandProtectionPolicy::Flatten { trigger, recover } => {
            BandProtectionPolicy::Flatten {
                trigger: match trigger {
                    poise_core::strategy::BandFlattenTrigger::Immediate => {
                        BandFlattenTrigger::Immediate
                    }
                    poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps } => {
                        BandFlattenTrigger::FlattenConfirm { bps }
                    }
                },
                recover: match recover {
                    poise_core::strategy::BandRecoverPolicy::BackInBand => {
                        BandRecoverPolicy::BackInBand
                    }
                    poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps } => {
                        BandRecoverPolicy::ReentryConfirm { bps }
                    }
                },
            }
        }
    }
}

pub(crate) fn project_risk_acquisition(
    value: poise_core::strategy::RiskAcquisitionConfig,
) -> RiskAcquisitionConfigView {
    RiskAcquisitionConfigView {
        initial_ratio: value.initial_ratio,
        advantage_steps: value.advantage_steps,
        min_release_steps: value.min_release_steps,
        max_release_steps: value.max_release_steps,
        catchup_ratio: value.catchup_ratio,
        stale_release_minutes: value.stale_release_minutes,
    }
}
