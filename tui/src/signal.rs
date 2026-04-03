use ratatui::style::Style;

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    Exposure,
    Pnl,
}

#[derive(Debug, Clone)]
pub struct SignalDisplay {
    pub text: String,
    pub style: Style,
}

pub fn exposure_signal(current: f64, target: Option<f64>) -> SignalDisplay {
    let delta = target.map(|target| target - current).unwrap_or(0.0);
    format_signal(delta, 4, SignalKind::Exposure)
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
