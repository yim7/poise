pub mod dashboard;
pub mod help;
pub mod instance;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, View};
use crate::theme::Theme;

pub fn render(app: &App, frame: &mut Frame<'_>) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header = Paragraph::new("grid-tui")
        .style(Theme::title())
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(header, areas[0]);

    match app.current_view {
        View::Dashboard => dashboard::render(frame, areas[1], app),
        View::Instance => instance::render(frame, areas[1], app),
        View::Help => help::render(frame, areas[1]),
    }

    let footer_text = app
        .status_message()
        .unwrap_or("q quit | arrows/jk move | Enter details | Esc back | ? help | p/r command");
    let footer = Paragraph::new(footer_text)
        .style(Theme::footer())
        .block(Block::default().borders(Borders::ALL).title("Keys"));
    frame.render_widget(footer, areas[2]);
}
