use ratatui::style::{Color, Modifier, Style};

use crate::protocol::InstanceStatus;

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

    pub fn table_header() -> Style {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }

    pub fn highlight() -> Style {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status(status: &InstanceStatus) -> Style {
        let color = match status {
            InstanceStatus::WaitingMarketData => Color::DarkGray,
            InstanceStatus::Active => Color::Green,
            InstanceStatus::Frozen | InstanceStatus::Holding => Color::Yellow,
            InstanceStatus::ReducingOnly => Color::LightYellow,
            InstanceStatus::Terminated | InstanceStatus::Paused => Color::Red,
        };

        Style::default().fg(color)
    }
}
