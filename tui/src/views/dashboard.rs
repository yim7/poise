use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};

use crate::app::App;
use crate::exposure_presentation::dashboard_exposure_summary;
use crate::protocol::{ExecutionStateView, ExecutionStatusView};
use crate::signal::{SignalDisplay, pnl_signal};
use crate::theme::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let header = Row::new(["ID", "Symbol", "Lifecycle", "Execution", "Exposure", "PnL"])
        .style(Theme::table_header());
    let rows = app.tracks.iter().map(|item| {
        let execution = format_execution_badge(
            item.execution.state,
            item.execution.execution_status,
            item.execution.active_slot_count,
        );
        let exposure = dashboard_exposure_summary(
            item.exposure.current,
            crate::signal::exposure_signal(item.exposure.current, item.exposure.target),
        );
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
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Fill(2),
            Constraint::Fill(1),
        ],
    )
    .header(header)
    .row_highlight_style(Theme::highlight())
    .highlight_symbol(">> ")
    .block(Block::default().title("Dashboard").borders(Borders::ALL));

    let mut state = TableState::default();
    if !app.tracks.is_empty() {
        state.select(Some(app.selected_index));
    }
    frame.render_stateful_widget(table, area, &mut state);
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
    use crate::protocol::ExecutionStatusView;

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

    fn substring_position(terminal: &Terminal<TestBackend>, needle: &str) -> Option<(u16, u16)> {
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
                    return Some((x, y));
                }
            }
        }

        None
    }

    fn background_colors_for_substring(terminal: &Terminal<TestBackend>, needle: &str) -> Vec<Color> {
        colors_for_substring(terminal, needle, |cell| cell.bg)
    }

    fn foreground_colors_for_substring(terminal: &Terminal<TestBackend>, needle: &str) -> Vec<Color> {
        colors_for_substring(terminal, needle, |cell| cell.fg)
    }

    fn colors_for_substring(
        terminal: &Terminal<TestBackend>,
        needle: &str,
        color_of: impl Fn(&ratatui::buffer::Cell) -> Color,
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
                        .map(|(offset, _)| color_of(&buffer.content()[start + offset]))
                        .collect();
                }
            }
        }

        Vec::new()
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
        assert!(text.contains("long add 0.5000"));
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
    fn keeps_signal_foreground_colors_on_selected_row() {
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

        assert!(
            foreground_colors_for_substring(&terminal, "long add 0.5000")
                .iter()
                .any(|fg| *fg == Color::Cyan)
        );
        assert!(
            foreground_colors_for_substring(&terminal, "↑ +1245.30")
                .iter()
                .any(|fg| *fg == Color::Green)
        );
        assert!(
            background_colors_for_substring(&terminal, "long add 0.5000")
                .iter()
                .all(|bg| *bg == Color::DarkGray)
        );
    }

    #[test]
    fn renders_short_add_signal_and_negative_pnl_in_dashboard() {
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
                        "exposure": {"current": -5.0, "target": -7.0},
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

        assert!(text.contains("-5.0000"));
        assert!(text.contains("short add 2.0000"));
        assert!(text.contains("↓ -245.30"));
    }

    #[test]
    fn keeps_long_exposure_text_separate_from_pnl() {
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
                        "exposure": {"current": -5.6430, "target": -5.3330},
                        "execution": {"state": "open", "execution_status": "normal", "active_slot_count": 1},
                        "statistics": {"total_pnl": 3.35, "realized_pnl": 0.0}
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

        assert!(text.contains("-5.6430"));
        assert!(text.contains("short reduce 0.3100"));
        assert!(text.contains("↑ +3.35"));
    }

    #[test]
    fn keeps_pnl_visible_when_dashboard_width_is_compact() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "reference_price": 101.25,
                        "exposure": {"current": -5.6430, "target": -5.3330},
                        "execution": {"state": "open", "execution_status": "normal", "active_slot_count": 1},
                        "statistics": {"total_pnl": 3.35, "realized_pnl": 0.0}
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

        assert!(text.contains("↑ +3.35"));
    }

    #[test]
    fn shares_remaining_width_between_exposure_and_pnl() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "reference_price": 101.25,
                        "exposure": {"current": -5.6430, "target": -5.3330},
                        "execution": {"state": "open", "execution_status": "normal", "active_slot_count": 1},
                        "statistics": {"total_pnl": 6.69, "realized_pnl": 0.0}
                    }
                ]
            }"#,
        )
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        let exposure_x = substring_position(&terminal, "Exposure").unwrap().0;
        let pnl_x = substring_position(&terminal, "PnL").unwrap().0;

        assert!(pnl_x > exposure_x);
        assert!(pnl_x < 100);
    }

    #[test]
    fn keeps_selected_waiting_and_neutral_text_readable() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "waiting_market_data", "updated_at": "2026-03-26T10:00:00Z"},
                        "reference_price": 101.25,
                        "exposure": {"current": 3.5, "target": null},
                        "execution": {"state": "open", "execution_status": "normal", "active_slot_count": 0},
                        "statistics": {"total_pnl": 0.0, "realized_pnl": 0.0}
                    }
                ]
            }"#,
        )
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        assert!(
            foreground_colors_for_substring(&terminal, "waiting_market_data")
                .iter()
                .all(|fg| *fg != Color::DarkGray)
        );
        assert!(
            foreground_colors_for_substring(&terminal, "3.5000 | target -")
                .iter()
                .all(|fg| *fg != Color::DarkGray)
        );
    }
}
