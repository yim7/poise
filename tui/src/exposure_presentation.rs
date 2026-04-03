use crate::signal::{
    ExposureAction, ExposureSide, ExposureSignal, SignalDisplay, SignalKind, SignalTone,
};
use crate::theme::Theme;

pub fn dashboard_exposure_summary(current: f64, signal: ExposureSignal) -> SignalDisplay {
    let text = if matches!(signal.action, ExposureAction::Unavailable) {
        format!("{current:.4} | target -")
    } else {
        format!("{current:.4} | {}", format_exposure_phrase(signal))
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
        format!("[{}]", format_exposure_phrase(signal))
    };

    SignalDisplay {
        text,
        style: exposure_style(signal.tone),
    }
}

fn format_exposure_phrase(signal: ExposureSignal) -> String {
    match signal.action {
        ExposureAction::Flip => format!(
            "{}->{} {} {:.4}",
            side_label(signal.current_side),
            side_label(signal.target_side),
            action_label(signal.action),
            signal.magnitude
        ),
        ExposureAction::Add | ExposureAction::Hold => format!(
            "{} {} {:.4}",
            side_label(signal.target_side),
            action_label(signal.action),
            signal.magnitude
        ),
        ExposureAction::Reduce => format!(
            "{} {} {:.4}",
            side_label(signal.current_side),
            action_label(signal.action),
            signal.magnitude
        ),
        ExposureAction::Unavailable => "target unavailable".to_string(),
    }
}

fn side_label(side: ExposureSide) -> &'static str {
    match side {
        ExposureSide::Long => "long",
        ExposureSide::Short => "short",
        ExposureSide::Flat => "flat",
    }
}

fn action_label(action: ExposureAction) -> &'static str {
    match action {
        ExposureAction::Add => "add",
        ExposureAction::Reduce => "reduce",
        ExposureAction::Flip => "flip",
        ExposureAction::Hold => "hold",
        ExposureAction::Unavailable => "unavailable",
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

        assert_eq!(display.text, "-5.6430 | short reduce 0.3100");
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

        assert_eq!(display.text, "[long add 0.5000]");
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
