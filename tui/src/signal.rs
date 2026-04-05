use ratatui::style::Style;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    Exposure,
    Pnl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalTone {
    Positive,
    Negative,
    Neutral,
    Accent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureSide {
    Long,
    Short,
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureAction {
    Add,
    Reduce,
    Flip,
    Hold,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureSignal {
    pub current_side: ExposureSide,
    pub target_side: ExposureSide,
    pub action: ExposureAction,
    pub magnitude: f64,
    pub tone: SignalTone,
}

#[derive(Debug, Clone)]
pub struct SignalDisplay {
    pub text: String,
    pub style: Style,
}

pub fn exposure_signal(current: f64, target: Option<f64>) -> ExposureSignal {
    format_exposure_signal(current, target)
}

pub fn pnl_signal(value: f64) -> SignalDisplay {
    format_signal(value, 2, SignalKind::Pnl)
}

fn format_signal(value: f64, precision: usize, kind: SignalKind) -> SignalDisplay {
    let threshold = 0.5 * 10f64.powi(-(precision as i32));

    if value > threshold {
        SignalDisplay {
            text: format!("↑ +{:.*}", precision, value),
            style: Theme::signal_positive(kind),
        }
    } else if value < -threshold {
        SignalDisplay {
            text: format!("↓ -{:.*}", precision, value.abs()),
            style: Theme::signal_negative(kind),
        }
    } else {
        SignalDisplay {
            text: format!("→ {:.*}", precision, 0.0),
            style: Theme::signal_neutral(),
        }
    }
}

fn format_exposure_signal(current: f64, target: Option<f64>) -> ExposureSignal {
    let threshold = 0.5 * 10f64.powi(-4);
    let Some(target) = target else {
        let side = exposure_side(current, threshold);
        return ExposureSignal {
            current_side: side,
            target_side: side,
            action: ExposureAction::Unavailable,
            magnitude: 0.0,
            tone: SignalTone::Neutral,
        };
    };

    let current_abs = current.abs();
    let target_abs = target.abs();
    let current_side = exposure_side(current, threshold);
    let target_side = exposure_side(target, threshold);
    let flips_direction =
        current_abs > threshold && target_abs > threshold && current.signum() != target.signum();

    if flips_direction {
        ExposureSignal {
            current_side,
            target_side,
            action: ExposureAction::Flip,
            magnitude: current_abs + target_abs,
            tone: SignalTone::Accent,
        }
    } else if target_abs > current_abs + threshold {
        ExposureSignal {
            current_side,
            target_side,
            action: ExposureAction::Add,
            magnitude: target_abs - current_abs,
            tone: SignalTone::Positive,
        }
    } else if current_abs > target_abs + threshold {
        ExposureSignal {
            current_side,
            target_side,
            action: ExposureAction::Reduce,
            magnitude: current_abs - target_abs,
            tone: SignalTone::Negative,
        }
    } else {
        ExposureSignal {
            current_side,
            target_side,
            action: ExposureAction::Hold,
            magnitude: 0.0,
            tone: SignalTone::Neutral,
        }
    }
}

fn exposure_side(value: f64, threshold: f64) -> ExposureSide {
    if value > threshold {
        ExposureSide::Long
    } else if value < -threshold {
        ExposureSide::Short
    } else {
        ExposureSide::Flat
    }
}

#[cfg(test)]
mod tests {
    use super::{ExposureAction, ExposureSide, SignalTone, exposure_signal};

    #[test]
    fn treats_short_size_growth_as_add() {
        let signal = exposure_signal(-5.0, Some(-7.0));

        assert_eq!(signal.current_side, ExposureSide::Short);
        assert_eq!(signal.target_side, ExposureSide::Short);
        assert_eq!(signal.action, ExposureAction::Add);
        assert_eq!(signal.magnitude, 2.0);
        assert_eq!(signal.tone, SignalTone::Positive);
    }

    #[test]
    fn treats_short_size_shrink_as_reduce() {
        let signal = exposure_signal(-5.0, Some(-3.0));

        assert_eq!(signal.current_side, ExposureSide::Short);
        assert_eq!(signal.target_side, ExposureSide::Short);
        assert_eq!(signal.action, ExposureAction::Reduce);
        assert_eq!(signal.magnitude, 2.0);
        assert_eq!(signal.tone, SignalTone::Negative);
    }

    #[test]
    fn treats_crossing_zero_as_flip() {
        let signal = exposure_signal(5.0, Some(-3.0));

        assert_eq!(signal.current_side, ExposureSide::Long);
        assert_eq!(signal.target_side, ExposureSide::Short);
        assert_eq!(signal.action, ExposureAction::Flip);
        assert_eq!(signal.magnitude, 8.0);
        assert_eq!(signal.tone, SignalTone::Accent);
    }

    #[test]
    fn treats_missing_target_as_unavailable() {
        let signal = exposure_signal(5.0, None);

        assert_eq!(signal.current_side, ExposureSide::Long);
        assert_eq!(signal.action, ExposureAction::Unavailable);
        assert_eq!(signal.tone, SignalTone::Neutral);
    }
}
