use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::exposure_presentation::dashboard_exposure_summary;
use crate::protocol::{
    ExecutionStateView, ExecutionStatusView, StrategyPriceStatusView, TrackStatus,
};
use crate::signal::{SignalDisplay, exposure_signal, pnl_signal};
use crate::theme::Theme;
use crate::views::account_panel;

const DASHBOARD_COLUMN_SPACING: u16 = 1;
const DASHBOARD_TABLE_CHROME_WIDTH: u16 = 3;
const DASHBOARD_ID_COLUMN_MAX_WIDTH: u16 = 8;
const DASHBOARD_SYMBOL_COLUMN_MAX_WIDTH: u16 = 13;
const DASHBOARD_TRAILING_COLUMN_WIDTHS: [u16; 5] = [18, 15, 13, 24, 11];
const DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS: [u16; 5] = [6, 4, 9, 4, 4];
const DASHBOARD_COLUMN_SHRINK_ORDER: [usize; 5] = [4, 2, 5, 3, 6];

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    account_panel::render(frame, sections[0], app.account_summary());

    let header = Row::new([
        "ID",
        "Symbol",
        "Lifecycle",
        "Execution",
        "Price",
        "Exposure",
        "PnL",
    ])
    .style(Theme::table_header());
    let rows = app.tracks.iter().map(|item| {
        let execution = format_execution_badge(
            item.execution.state,
            item.execution.execution_status,
            item.execution.active_binding_count,
        );
        let price = format_price_summary(item.strategy_price, item.strategy_price_status.clone());
        let exposure = format_exposure_summary(item.exposure.current, item.exposure.target);
        let total_pnl = pnl_signal(item.ledger.total_pnl);

        Row::new(vec![
            Cell::from(item.id.clone()),
            Cell::from(item.instrument.symbol.clone()),
            Cell::from(format_lifecycle_label(&item.lifecycle.status))
                .style(Theme::status(&item.lifecycle.status)),
            Cell::from(execution.text).style(execution.style),
            Cell::from(price.text).style(price.style),
            Cell::from(exposure.text).style(exposure.style),
            Cell::from(total_pnl.text).style(total_pnl.style),
        ])
    });

    let table = Table::new(rows, dashboard_column_constraints(sections[1].width, app))
        .header(header)
        .column_spacing(DASHBOARD_COLUMN_SPACING)
        .row_highlight_style(Theme::highlight())
        .highlight_symbol(">")
        .block(Block::default().title("Dashboard").borders(Borders::ALL));

    let mut state = TableState::default();
    if !app.tracks.is_empty() {
        state.select(Some(app.selected_index));
    }
    frame.render_stateful_widget(table, sections[1], &mut state);
}

fn dashboard_column_constraints(table_width: u16, app: &App) -> [Constraint; 7] {
    let mut widths = [
        dashboard_key_column_width(
            app.tracks.iter().map(|item| item.id.as_str()),
            "ID",
            DASHBOARD_ID_COLUMN_MAX_WIDTH,
        ),
        dashboard_key_column_width(
            app.tracks
                .iter()
                .map(|item| item.instrument.symbol.as_str()),
            "Symbol",
            DASHBOARD_SYMBOL_COLUMN_MAX_WIDTH,
        ),
        DASHBOARD_TRAILING_COLUMN_WIDTHS[0],
        DASHBOARD_TRAILING_COLUMN_WIDTHS[1],
        DASHBOARD_TRAILING_COLUMN_WIDTHS[2],
        DASHBOARD_TRAILING_COLUMN_WIDTHS[3],
        DASHBOARD_TRAILING_COLUMN_WIDTHS[4],
    ];
    let min_widths = [
        widths[0],
        widths[1],
        DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS[0],
        DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS[1],
        DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS[2],
        DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS[3],
        DASHBOARD_TRAILING_COLUMN_MIN_WIDTHS[4],
    ];
    let available_width = table_width
        .saturating_sub(DASHBOARD_TABLE_CHROME_WIDTH)
        .saturating_sub(DASHBOARD_COLUMN_SPACING * (widths.len().saturating_sub(1) as u16));
    let mut deficit = widths.iter().sum::<u16>().saturating_sub(available_width);

    for index in DASHBOARD_COLUMN_SHRINK_ORDER {
        if deficit == 0 {
            break;
        }
        let min_width = min_widths[index];
        let shrinkable = widths[index].saturating_sub(min_width);
        let reduction = shrinkable.min(deficit);
        widths[index] = widths[index].saturating_sub(reduction);
        deficit = deficit.saturating_sub(reduction);
    }

    widths.map(Constraint::Length)
}

fn dashboard_key_column_width<'a>(
    values: impl Iterator<Item = &'a str>,
    header: &str,
    max_width: u16,
) -> u16 {
    values
        .map(display_width)
        .fold(display_width(header), usize::max)
        .min(max_width as usize) as u16
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn format_exposure_summary(current: f64, target: Option<f64>) -> SignalDisplay {
    dashboard_exposure_summary(current, exposure_signal(current, target))
}

fn format_lifecycle_label(status: &TrackStatus) -> &'static str {
    match status {
        TrackStatus::WaitingMarketData => "waiting",
        TrackStatus::Active => "active",
        TrackStatus::Frozen => "frozen",
        TrackStatus::Flattening => "flattening",
        TrackStatus::ManualFlattening => "manual_flattening",
        TrackStatus::Terminated => "terminated",
        TrackStatus::Paused => "paused",
    }
}

fn format_price_summary(
    strategy_price: Option<f64>,
    strategy_price_status: StrategyPriceStatusView,
) -> SignalDisplay {
    match strategy_price {
        Some(price) => match strategy_price_status {
            StrategyPriceStatusView::Live => SignalDisplay {
                text: format!("{price:.4}"),
                style: Theme::price_fresh(),
            },
            StrategyPriceStatusView::Stale => SignalDisplay {
                text: format!("{price:.4}?"),
                style: Theme::price_stale(),
            },
        },
        None => SignalDisplay {
            text: "--".to_string(),
            style: Theme::price_stale(),
        },
    }
}

fn format_execution_badge(
    state: ExecutionStateView,
    execution_status: ExecutionStatusView,
    active_binding_count: u32,
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
    let text = if active_binding_count > 0 {
        format!("{badge} ({active_binding_count})")
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
    use crate::protocol::{AccountSummaryView, ExecutionStatusView, StrategyPriceStatusView};

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

    fn buffer_line_containing(terminal: &Terminal<TestBackend>, needle: &str) -> Option<String> {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;

        (0..buffer.area.height as usize).find_map(|row| {
            let start = row * width;
            let line = buffer.content()[start..start + width]
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            line.contains(needle).then_some(line)
        })
    }

    fn compact_text(text: &str) -> String {
        text.chars().filter(|ch| *ch != ' ').collect()
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
                    buffer.content()[start + offset]
                        .symbol()
                        .starts_with(*expected)
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

    fn foreground_colors_for_substring(
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
                    buffer.content()[start + offset]
                        .symbol()
                        .starts_with(*expected)
                });
                if matches {
                    return needle_chars
                        .iter()
                        .enumerate()
                        .map(|(offset, _)| buffer.content()[start + offset].fg)
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
        assert!(text.contains("3.5000 | ↑ +0.5000"));
        assert!(text.contains("↑ +1229.00"));
        assert!(text.contains("Execution"));
        assert!(text.contains("open"));
    }

    #[test]
    fn renders_price_column_with_strategy_price_status() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut extra = response.items[0].clone();
        extra.id = "eth-core".to_string();
        extra.instrument.symbol = "ETHUSDT".to_string();
        extra.strategy_price = Some(88.88);
        response.items.push(extra);
        let mut app = App::new(response.items);
        app.select_next();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("Price"));
        assert!(text.contains("101.2500"));
        assert!(!text.contains("101.2500 live"));
        assert!(
            foreground_colors_for_substring(&terminal, "101.2500")
                .iter()
                .all(|fg| *fg == Color::Green)
        );
    }

    #[test]
    fn renders_stale_strategy_price_with_short_marker_and_stale_color() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        response.items[0].strategy_price = Some(101.25);
        response.items[0].strategy_price_status = StrategyPriceStatusView::Stale;
        let mut extra = response.items[0].clone();
        extra.id = "eth-core".to_string();
        extra.instrument.symbol = "ETHUSDT".to_string();
        extra.strategy_price = Some(88.88);
        extra.strategy_price_status = StrategyPriceStatusView::Live;
        response.items.push(extra);
        let mut app = App::new(response.items);
        app.select_next();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("101.2500?"));
        assert!(
            foreground_colors_for_substring(&terminal, "101.2500?")
                .iter()
                .all(|fg| *fg == Color::Yellow)
        );
    }

    #[test]
    fn renders_missing_strategy_price_as_stale_placeholder() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "strategy_price": null,
                        "strategy_price_status": "live",
                        "exposure": {"current": 3.5, "target": 3.0},
                        "execution": {"state": "open", "execution_status": "normal", "active_binding_count": 0},
                        "ledger": {"total_pnl": 12.3, "has_unresolved_gaps": false}
                    }
                ]
            }"#,
        )
        .unwrap();
        let mut extra = response.items[0].clone();
        extra.id = "eth-core".to_string();
        extra.instrument.symbol = "ETHUSDT".to_string();
        extra.strategy_price = Some(101.25);
        extra.strategy_price_status = StrategyPriceStatusView::Live;
        response.items.push(extra);
        let mut app = App::new(response.items);
        app.select_next();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("--"));
        assert!(
            foreground_colors_for_substring(&terminal, "--")
                .iter()
                .all(|fg| *fg == Color::Yellow)
        );
    }

    #[test]
    fn renders_manual_flattening_lifecycle_label() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        response.items[0].lifecycle.status = serde_json::from_str("\"manual_flattening\"").unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("manual_flattening"));
    }

    #[test]
    fn renders_flattening_without_holding_status() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        response.items[0].lifecycle.status = serde_json::from_str("\"flattening\"").unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let text = buffer_text(&terminal);

        assert!(text.contains("flattening"));
        assert!(!text.contains("holding"));
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
    fn keeps_execution_and_pnl_visible_with_price_column() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "btc-core",
                        "instrument": {"venue": "binance_futures", "symbol": "BTCUSDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "strategy_price": 101.25,
                        "strategy_price_status": "live",
                        "exposure": {"current": 3.5, "target": 3.0},
                        "execution": {"state": "open", "execution_status": "attention_required", "active_binding_count": 1},
                        "ledger": {"total_pnl": 12345.67, "has_unresolved_gaps": false}
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

        assert!(text.contains("! ATTN open (1)"));
        assert!(text.contains("↑ +12345.67"));
    }

    #[test]
    fn keeps_full_cjk_id_and_symbol_visible_when_dashboard_is_narrow() {
        let backend = TestBackend::new(70, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(
            r#"{
                "items": [
                    {
                        "id": "币安人生",
                        "instrument": {"venue": "binance_futures", "symbol": "币安人生 USDT"},
                        "lifecycle": {"status": "active", "updated_at": "2026-03-26T10:00:00Z"},
                        "strategy_price": 101.25,
                        "strategy_price_status": "live",
                        "exposure": {"current": 3.5, "target": 3.0},
                        "execution": {"state": "open", "execution_status": "normal", "active_binding_count": 0},
                        "ledger": {"total_pnl": 12.3, "has_unresolved_gaps": false}
                    }
                ]
            }"#,
        )
        .unwrap();
        let app = App::new(response.items);

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        let row = compact_text(&buffer_line_containing(&terminal, "active").unwrap());
        assert!(
            row.contains(&compact_text("币安人生")),
            "row should keep full CJK id visible, line: {row:?}"
        );
        assert!(
            row.contains(&compact_text("币安人生 USDT")),
            "row should keep full mixed-width symbol visible, line: {row:?}"
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
                        "strategy_price": 101.25,
                        "strategy_price_status": "live",
                        "exposure": {"current": 3.5, "target": 3.0},
                        "execution": {"state": "open", "execution_status": "normal", "active_binding_count": 0},
                        "ledger": {"total_pnl": -245.3, "has_unresolved_gaps": false}
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

        assert!(text.contains("3.5000 | ↓ -0.5000"));
        assert!(text.contains("↓ -245.30"));
    }

    #[test]
    fn keeps_pnl_column_visibly_separated_from_exposure_column() {
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

        let header = buffer_line_containing(&terminal, "Exposure").unwrap();
        let header_gap = header.find("PnL").unwrap() - header.find("Exposure").unwrap();
        assert!(
            header_gap >= 24,
            "header should keep Exposure and PnL apart, line: {header:?}"
        );

        let row = buffer_line_containing(&terminal, "BTCUSDT").unwrap();
        let row_gap = row.find("↑ +1229.00").unwrap() - row.find("3.5000").unwrap();
        assert!(
            row_gap >= 24,
            "row should keep Exposure and PnL apart, line: {row:?}"
        );
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
