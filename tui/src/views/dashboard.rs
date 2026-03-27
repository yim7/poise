use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, Borders, Row, Table, TableState};

use crate::app::App;
use crate::protocol::ExecutionStateView;
use crate::theme::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let header = Row::new([
        "ID",
        "Symbol",
        "Lifecycle",
        "Execution",
        "Exposure",
        "Last Price",
    ])
    .style(Theme::table_header());
    let rows = app.grids.iter().map(|item| {
        let exposure = format!("{:.4}", item.exposure.current);
        let reference_price = item
            .reference_price
            .map(|value| format!("{value:.4}"))
            .unwrap_or_else(|| "-".to_string());
        let execution =
            format_execution_badge(item.execution.state, item.execution.pending_order_count);

        Row::new(vec![
            item.id.clone(),
            item.instrument.symbol.clone(),
            item.lifecycle.status.to_string(),
            execution,
            exposure,
            reference_price,
        ])
        .style(Theme::status(&item.lifecycle.status))
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(14),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(16),
        ],
    )
    .header(header)
    .row_highlight_style(Theme::highlight())
    .highlight_symbol(">> ")
    .block(Block::default().title("Dashboard").borders(Borders::ALL));

    let mut state = TableState::default();
    if !app.grids.is_empty() {
        state.select(Some(app.selected_index));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn format_execution_badge(state: ExecutionStateView, pending_order_count: u32) -> String {
    let state = match state {
        ExecutionStateView::Open => "open",
        ExecutionStateView::Paused => "paused",
        ExecutionStateView::Closed => "closed",
    };

    if pending_order_count == 0 {
        state.to_string()
    } else {
        format!("{state} ({pending_order_count})")
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
    fn renders_dashboard_rows_from_grid_list_items() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::GridListResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/grid_list_response.json"))
                .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Dashboard"));
        assert!(text.contains("BTCUSDT"));
        assert!(text.contains("1.2500"));
        assert!(text.contains("Execution"));
        assert!(text.contains("open (1)"));
    }
}
