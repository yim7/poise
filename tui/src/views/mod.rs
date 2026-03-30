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

    let header = Paragraph::new("Poise")
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::App;

    use super::render;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn renders_poise_header() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::GridListResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/grid_list_response.json"))
                .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(&app, frame))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Poise"));
        assert!(text.contains("Status"));
        assert!(text.contains("Keys"));
    }
}
