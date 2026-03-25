use ratatui::style::{Color, Modifier, Style};

use crate::protocol::GridStatus;

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

    pub fn status(status: &GridStatus) -> Style {
        let color = match status {
            GridStatus::WaitingMarketData => Color::DarkGray,
            GridStatus::Active => Color::Green,
            GridStatus::Frozen | GridStatus::Holding => Color::Yellow,
            GridStatus::ReducingOnly => Color::LightYellow,
            GridStatus::Terminated | GridStatus::Paused => Color::Red,
        };

        Style::default().fg(color)
    }
}
