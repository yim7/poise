use crate::signal::{
    ExposureAction, ExposureSignal, SignalDisplay, SignalKind, SignalTone,
};
use crate::theme::Theme;

pub fn dashboard_exposure_summary(current: f64, signal: ExposureSignal) -> SignalDisplay {
    let text = if matches!(signal.action, ExposureAction::Unavailable) {
        format!("{current:.4} | target -")
    } else {
        format!("{current:.4} | {}", format_exposure_delta(signal))
    };

    SignalDisplay {
        text,
        style: exposure_style(signal.tone),
    }
}

pub fn instance_exposure_annotation(signal: ExposureSignal) -> SignalDisplay {
    let text = if matches!(signal.action, ExposureAction::Unavailable) {
        "[target unavailable]".to_string()
    } else {
        format!("[{}]", format_exposure_delta(signal))
    };

    SignalDisplay {
        text,
        style: exposure_style(signal.tone),
    }
}

fn format_exposure_delta(signal: ExposureSignal) -> String {
    match signal.action {
        ExposureAction::Flip => format!("⇄ {:.4}", signal.magnitude),
        ExposureAction::Add => format!("↑ +{:.4}", signal.magnitude),
        ExposureAction::Reduce => format!("↓ -{:.4}", signal.magnitude),
        ExposureAction::Hold => "→ 0.0000".to_string(),
        ExposureAction::Unavailable => "target unavailable".to_string(),
    }
}

fn exposure_style(tone: SignalTone) -> ratatui::style::Style {
    match tone {
        SignalTone::Positive => Theme::signal_positive(SignalKind::Exposure),
        SignalTone::Negative => Theme::signal_negative(SignalKind::Exposure),
        SignalTone::Neutral => Theme::signal_neutral(),
        SignalTone::Accent => Theme::signal_flip(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use crate::signal::{ExposureAction, ExposureSide, ExposureSignal, SignalTone};

    use super::{dashboard_exposure_summary, instance_exposure_annotation};

    #[test]
    fn dashboard_formats_full_english_phrase_from_semantics() {
        let display = dashboard_exposure_summary(
            -5.6430,
            ExposureSignal {
                current_side: ExposureSide::Short,
                target_side: ExposureSide::Short,
                action: ExposureAction::Reduce,
                magnitude: 0.31,
                tone: SignalTone::Negative,
            },
        );

        assert_eq!(display.text, "-5.6430 | ↓ -0.3100");
        assert_eq!(display.style.fg, Some(Color::LightYellow));
    }

    #[test]
    fn instance_formats_annotation_from_semantics() {
        let display = instance_exposure_annotation(ExposureSignal {
            current_side: ExposureSide::Long,
            target_side: ExposureSide::Long,
            action: ExposureAction::Add,
            magnitude: 0.5,
            tone: SignalTone::Positive,
        });

        assert_eq!(display.text, "[↑ +0.5000]");
        assert_eq!(display.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn formats_unavailable_target_without_hold_wording() {
        let dashboard = dashboard_exposure_summary(
            3.5,
            ExposureSignal {
                current_side: ExposureSide::Long,
                target_side: ExposureSide::Long,
                action: ExposureAction::Unavailable,
                magnitude: 0.0,
                tone: SignalTone::Neutral,
            },
        );
        let instance = instance_exposure_annotation(ExposureSignal {
            current_side: ExposureSide::Long,
            target_side: ExposureSide::Long,
            action: ExposureAction::Unavailable,
            magnitude: 0.0,
            tone: SignalTone::Neutral,
        });

        assert_eq!(dashboard.text, "3.5000 | target -");
        assert_eq!(instance.text, "[target unavailable]");
    }
}
