use chrono::{DateTime, Utc};
use poise_core::events::DomainEvent;
use poise_engine::ports::EffectStatus;
use poise_engine::transition::TrackEffect;
use poise_protocol::ActivityLevelView;

use crate::read_model::TrackReadModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentationAudience {
    Activity,
    Diagnostics,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PresentedEvent {
    pub ts: DateTime<Utc>,
    pub message: String,
    pub level: ActivityLevelView,
    pub audience: PresentationAudience,
}

pub fn classify_track_events(source: &TrackReadModel) -> Vec<PresentedEvent> {
    let mut items = Vec::new();

    for event in &source.recent_track_events {
        let audience = match event.event {
            DomainEvent::ExposureTargetChanged { .. } => PresentationAudience::Diagnostics,
            _ => PresentationAudience::Activity,
        };

        items.push(PresentedEvent {
            ts: event.created_at,
            message: project_domain_event_message(&event.event),
            level: project_domain_event_level(&event.event),
            audience,
        });
    }

    for effect in &source.recent_effects {
        items.push(PresentedEvent {
            ts: effect.updated_at,
            message: project_effect_message(effect),
            level: project_effect_level(effect.status),
            audience: PresentationAudience::Activity,
        });
    }

    items.sort_by_key(|item| item.ts);
    items
}

fn project_domain_event_message(event: &DomainEvent) -> String {
    match event {
        DomainEvent::ExposureTargetChanged { from, to } => {
            format!("target exposure {:.4} -> {:.4}", from.0, to.0)
        }
        DomainEvent::BandBreached { boundary, price } => {
            format!("band breached {:?} at {:.4}", boundary, price)
        }
        DomainEvent::BandReentered { price } => format!("band reentered at {:.4}", price),
        DomainEvent::PolicyTriggered { policy } => format!("policy triggered: {:?}", policy),
        DomainEvent::RiskCapApplied { intended, capped } => {
            format!("risk cap {:.4} -> {:.4}", intended.0, capped.0)
        }
        DomainEvent::RiskDenied { reason } => format!("risk denied: {reason}"),
        DomainEvent::ReplacementGateApplied { reason } => match reason {
            poise_core::events::ReplacementGateReason::RoundedMatch => {
                "replacement gate: candidate matches working order after rounding".into()
            }
            poise_core::events::ReplacementGateReason::ImprovementBelowThreshold {
                improvement_bps,
                threshold_bps,
            } => format!(
                "replacement gate: improvement {:.1} bps < threshold {:.1} bps",
                improvement_bps, threshold_bps
            ),
        },
    }
}

fn project_domain_event_level(event: &DomainEvent) -> ActivityLevelView {
    match event {
        DomainEvent::RiskDenied { .. } => ActivityLevelView::Warn,
        _ => ActivityLevelView::Info,
    }
}

fn project_effect_message(effect: &poise_engine::ports::PersistedTrackEffect) -> String {
    match &effect.effect {
        TrackEffect::SubmitOrder { .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| "submit order failed".into()),
            EffectStatus::Succeeded => "submit order succeeded".into(),
            EffectStatus::Superseded => "submit order superseded by newer grid state".into(),
            EffectStatus::Executing => "submit order executing".into(),
            EffectStatus::Pending => "submit order pending".into(),
        },
        TrackEffect::CancelOrder { order_id, .. } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel {order_id} failed")),
            EffectStatus::Succeeded => format!("cancel {order_id} succeeded"),
            EffectStatus::Superseded => format!("cancel {order_id} superseded"),
            EffectStatus::Executing => format!("cancel {order_id} executing"),
            EffectStatus::Pending => format!("cancel {order_id} pending"),
        },
        TrackEffect::CancelAll { instrument } => match effect.status {
            EffectStatus::Failed => effect
                .last_error
                .clone()
                .unwrap_or_else(|| format!("cancel all {} failed", instrument.symbol)),
            EffectStatus::Succeeded => format!("cancel all {} succeeded", instrument.symbol),
            EffectStatus::Superseded => format!("cancel all {} superseded", instrument.symbol),
            EffectStatus::Executing => format!("cancel all {} executing", instrument.symbol),
            EffectStatus::Pending => format!("cancel all {} pending", instrument.symbol),
        },
        TrackEffect::NoOp => "no-op".into(),
    }
}

fn project_effect_level(status: EffectStatus) -> ActivityLevelView {
    match status {
        EffectStatus::Failed => ActivityLevelView::Error,
        _ => ActivityLevelView::Info,
    }
}
