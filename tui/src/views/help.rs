use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn render(frame: &mut Frame<'_>, area: Rect) {
    let lines = vec![
        Line::from("q        quit"),
        Line::from("up/k     move selection up"),
        Line::from("down/j   move selection down"),
        Line::from("Enter    open instance details"),
        Line::from("Esc      back to dashboard"),
        Line::from("[ / ]    switch instance"),
        Line::from("?        open help"),
        Line::from("p / r    pause or resume current instance"),
    ];
    let help = Paragraph::new(lines).block(Block::default().title("Help").borders(Borders::ALL));
    frame.render_widget(help, area);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
    fn renders_help_shortcuts() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, frame.area())).unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Help"));
        assert!(text.contains("pause or resume"));
        assert!(text.contains("switch instance"));
    }
}
