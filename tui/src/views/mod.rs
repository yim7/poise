pub mod dashboard;
pub mod help;
pub mod instance;
pub mod instance_layout;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, View};
use crate::theme::Theme;

const KEY_HINTS: &str = "q quit | arrows/jk move | Enter details | Esc back | ? help | p/r command";

pub fn render(app: &App, frame: &mut Frame<'_>) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header = Paragraph::new(render_status_line(app))
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(header, areas[0]);

    match app.current_view {
        View::Dashboard => dashboard::render(frame, areas[1], app),
        View::Instance => instance::render(frame, areas[1], app),
        View::Help => help::render(frame, areas[1]),
    }

    let footer = Paragraph::new(KEY_HINTS)
        .style(Theme::footer())
        .block(Block::default().borders(Borders::ALL).title("Keys"));
    frame.render_widget(footer, areas[2]);
}

fn render_status_line(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled("Poise", Theme::title())];

    if let Some(message) = app.status_message() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(message.to_string(), Theme::status_value()));
    }

    spans.push(Span::raw(" | "));
    spans.push(Span::styled(
        view_label(app.current_view),
        Theme::status_context(),
    ));

    if let Some(grid) = app.selected_grid() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            format!("{} / {}", grid.id, grid.instrument.symbol),
            Theme::status_context(),
        ));
    }

    if app.selected_execution_status() == Some(crate::protocol::ExecutionStatusView::AttentionRequired) {
        if let Some(track_id) = app.selected_track_id() {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                format!("! execution anomaly on {track_id}"),
                Theme::status_alert(),
            ));
        }
    }

    Line::from(spans)
}
fn view_label(view: View) -> &'static str {
    match view {
        View::Dashboard => "dashboard",
        View::Instance => "instance",
        View::Help => "help",
    }
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
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let app = App::new(response.items);

        terminal.draw(|frame| render(&app, frame)).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Poise"));
        assert!(text.contains("Status"));
        assert!(text.contains("Keys"));
    }

    #[test]
    fn renders_runtime_status_in_header_and_keeps_keys_in_footer() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut app = App::new(response.items);
        app.set_status_message("websocket connected");

        terminal.draw(|frame| render(&app, frame)).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("websocket connected"));
        assert!(text.contains("q quit"));
    }

    #[test]
    fn renders_view_context_even_without_selected_grid() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new(vec![]);

        terminal.draw(|frame| render(&app, frame)).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("dashboard"));
    }
}
