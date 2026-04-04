use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};

use crate::app::App;
use crate::protocol::{ExecutionStateView, ExecutionStatusView};
use crate::signal::{SignalDisplay, exposure_signal, pnl_signal};
use crate::theme::Theme;
use crate::views::account_panel;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    account_panel::render(frame, sections[0], app.account_summary());

    let header = Row::new(["ID", "Symbol", "Lifecycle", "Execution", "Exposure", "PnL"])
        .style(Theme::table_header());
    let rows = app.grids.iter().map(|item| {
        let execution = format_execution_badge(
            item.execution.state,
            item.execution.execution_status,
            item.execution.active_slot_count,
        );
        let exposure = format_exposure_summary(item.exposure.current, item.exposure.target);
        let total_pnl = pnl_signal(item.statistics.total_pnl);

        Row::new(vec![
            Cell::from(item.id.clone()),
            Cell::from(item.instrument.symbol.clone()),
            Cell::from(item.lifecycle.status.to_string())
                .style(Theme::status(&item.lifecycle.status)),
            Cell::from(execution.text).style(execution.style),
            Cell::from(exposure.text).style(exposure.style),
            Cell::from(total_pnl.text).style(total_pnl.style),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(14),
            Constraint::Length(15),
            Constraint::Length(22),
            Constraint::Length(14),
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
    frame.render_stateful_widget(table, sections[1], &mut state);
}

fn format_exposure_summary(current: f64, target: Option<f64>) -> SignalDisplay {
    let signal = exposure_signal(current, target);

    SignalDisplay {
        text: format!("{current:.4} | {}", signal.text),
        style: signal.style,
    }
}

fn format_execution_badge(
    state: ExecutionStateView,
    execution_status: ExecutionStatusView,
    active_slot_count: u32,
) -> SignalDisplay {
    let state = match state {
        ExecutionStateView::Open => "open",
        ExecutionStateView::Paused => "paused",
        ExecutionStateView::Closed => "closed",
    };

    let badge = if matches!(execution_status, ExecutionStatusView::AttentionRequired) {
        format!("! ATTN {state}")
    } else {
        state.to_string()
    };
    let text = if active_slot_count > 0 {
        format!("{badge} ({active_slot_count})")
    } else {
        badge
    };

    let style = if matches!(execution_status, ExecutionStatusView::AttentionRequired) {
        Theme::execution_attention()
    } else {
        Theme::status_neutral()
    };

    SignalDisplay { text, style }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use crate::app::App;
    use crate::protocol::{AccountSummaryView, ExecutionStatusView};

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

    fn background_colors_for_substring(
        terminal: &Terminal<TestBackend>,
        needle: &str,
    ) -> Vec<Color> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let needle_chars: Vec<char> = needle.chars().collect();

        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let start = y as usize * width + x as usize;
                if x as usize + needle_chars.len() > width {
                    continue;
                }

                let matches = needle_chars.iter().enumerate().all(|(offset, expected)| {
                    buffer.content()[start + offset].symbol().chars().next() == Some(*expected)
                });
                if matches {
                    return needle_chars
                        .iter()
                        .enumerate()
                        .map(|(offset, _)| buffer.content()[start + offset].bg)
                        .collect();
                }
            }
        }

        Vec::new()
    }

    fn account_summary_view() -> AccountSummaryView {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/account_summary_view.json"
        ))
        .unwrap()
    }

    #[test]
    fn renders_dashboard_rows_from_track_list_items() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Dashboard"));
        assert!(text.contains("BTCUSDT"));
        assert!(text.contains("PnL"));
        assert!(text.contains("3.5000"));
        assert!(text.contains("↑ +0.5000"));
        assert!(text.contains("↑ +1245.30"));
        assert!(text.contains("Execution"));
        assert!(text.contains("open"));
    }

    #[test]
    fn renders_attention_badge_for_anomalous_track() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        response.items[0].execution.execution_status = ExecutionStatusView::AttentionRequired;
        let mut extra = response.items[0].clone();
        extra.id = "eth-core".to_string();
        extra.instrument.symbol = "ETHUSDT".to_string();
        response.items.push(extra);
        let mut app = App::new(response.items);
        app.select_next();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("! ATTN open"));
        assert!(
            background_colors_for_substring(&terminal, "! ATTN open")
                .iter()
                .any(|bg| *bg != Color::Reset)
        );
    }

    #[test]
    fn renders_reduce_signal_and_negative_pnl_in_dashboard() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "reference_price": 101.25,
                        "exposure": {"current": 3.5, "target": 3.0},
                        "execution": {"state": "open", "execution_status": "normal", "active_slot_count": 0},
                        "statistics": {"total_pnl": -245.3, "realized_pnl": -12.5}
                    }
                ]
            }"#,
        )
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("3.5000"));
        assert!(text.contains("↓ -0.5000"));
        assert!(text.contains("↓ -245.30"));
    }

    #[test]
    fn renders_account_panel_with_attention_signal() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut app = App::new(response.items);
        app.account_summary = Some(account_summary_view());

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Account"));
        assert!(text.contains("12,500.00"));
        assert!(text.contains("attention"));
        assert!(
            background_colors_for_substring(&terminal, "attention")
                .iter()
                .any(|bg| *bg != Color::Reset)
        );
    }

    #[test]
    fn renders_unavailable_account_panel_when_summary_is_missing() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("unavailable"));
        assert!(text.contains("waiting for account summary"));
    }
}
