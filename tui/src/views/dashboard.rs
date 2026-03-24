use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, Borders, Row, Table, TableState};

use crate::app::App;
use crate::theme::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let header =
        Row::new(["ID", "Symbol", "Status", "Exposure", "Last Price"]).style(Theme::table_header());
    let rows = app.instances.iter().map(|item| {
        let exposure = app
            .cached_snapshot(&item.id)
            .map(|snapshot| format!("{:.4}", snapshot.current_exposure))
            .unwrap_or_else(|| "-".to_string());
        let last_price = item
            .last_price
            .map(|value| format!("{value:.4}"))
            .unwrap_or_else(|| "-".to_string());

        Row::new(vec![
            item.id.clone(),
            item.symbol.clone(),
            item.status.to_string(),
            exposure,
            last_price,
        ])
        .style(Theme::status(&item.status))
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(14),
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
    if !app.instances.is_empty() {
        state.select(Some(app.selected_index));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::App;
    use crate::protocol::{
        GridConfig, InstanceSnapshot, InstanceStatus, InstanceSummary, OutOfBandPolicy, ShapeFamily,
    };

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
    fn renders_dashboard_rows() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(vec![InstanceSummary {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            last_price: Some(100.0),
        }]);
        app.apply_snapshot(InstanceSnapshot {
            id: "BTCUSDT".into(),
            symbol: "BTCUSDT".into(),
            status: InstanceStatus::Active,
            current_exposure: 1.25,
            last_price: Some(100.0),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
        });

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Dashboard"));
        assert!(text.contains("BTCUSDT"));
        assert!(text.contains("1.2500"));
    }
}
