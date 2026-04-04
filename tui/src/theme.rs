use ratatui::style::{Color, Modifier, Style};

use crate::protocol::TrackStatus;
use crate::signal::SignalKind;

pub struct Theme;

impl Theme {
    pub fn title() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }

    pub fn footer() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    pub fn status_value() -> Style {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_context() -> Style {
        Style::default().fg(Color::Cyan)
    }

    pub fn status_alert() -> Style {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    }

    pub fn table_header() -> Style {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }

    pub fn highlight() -> Style {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status(status: &TrackStatus) -> Style {
        let color = match status {
            TrackStatus::WaitingMarketData => Color::Gray,
            TrackStatus::Active => Color::LightGreen,
            TrackStatus::Frozen | TrackStatus::Holding => Color::Yellow,
            TrackStatus::ReducingOnly => Color::LightYellow,
            TrackStatus::Terminated | TrackStatus::Paused => Color::Red,
        };

        Style::default().fg(color)
    }

    pub fn execution_attention() -> Style {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_neutral() -> Style {
        Style::default().fg(Color::White)
    }

    pub fn signal_positive(kind: SignalKind) -> Style {
        match kind {
            SignalKind::Exposure => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            SignalKind::Pnl => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn signal_negative(kind: SignalKind) -> Style {
        match kind {
            SignalKind::Exposure => Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            SignalKind::Pnl => Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn signal_neutral() -> Style {
        Style::default().fg(Color::Gray)
    }

    pub fn signal_flip() -> Style {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use crate::protocol::TrackStatus;

    use super::Theme;

    #[test]
    fn uses_light_green_for_active_lifecycle() {
        assert_eq!(Theme::status(&TrackStatus::Active).fg, Some(Color::LightGreen));
    }

    #[test]
    fn uses_gray_for_waiting_lifecycle() {
        assert_eq!(Theme::status(&TrackStatus::WaitingMarketData).fg, Some(Color::Gray));
    }
}
