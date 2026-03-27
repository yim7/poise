use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::protocol::{
    ActivityLevelView, ExecutionStateView, GridCommandType, GridCommandView, GridExecutionView,
};

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Min(0),
        ])
        .split(area);

    let Some(detail) = app
        .current_grid_detail()
        .or_else(|| app.current_grid.as_ref())
    else {
        let empty = Paragraph::new("No grid detail loaded")
            .block(Block::default().title("Instance").borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };

    let summary_lines = vec![
        Line::from(format!(
            "id/symbol: {} / {}",
            detail.identity.id, detail.identity.instrument.symbol
        )),
        Line::from(format!("lifecycle: {}", detail.status.lifecycle.status)),
        Line::from(format!(
            "updated at: {}",
            detail.status.lifecycle.updated_at
        )),
        Line::from(format!(
            "reference/exposure: {} / {:.4} -> {}",
            detail
                .status
                .reference_price
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "-".to_string()),
            detail.position.current_exposure,
            detail
                .position
                .target_exposure
                .map(|value| format!("{value:.4}"))
                .unwrap_or_else(|| "-".to_string())
        )),
    ];
    let summary = Paragraph::new(summary_lines)
        .block(Block::default().title("Overview").borders(Borders::ALL));
    frame.render_widget(summary, sections[0]);

    let statistics_lines = vec![
        Line::from("Total PnL | Realized PnL"),
        Line::from(format!(
            "{} | {}",
            format_pnl(detail.statistics.total_pnl),
            format_pnl(detail.statistics.realized_pnl),
        )),
    ];
    let statistics = Paragraph::new(statistics_lines)
        .block(Block::default().title("Statistics").borders(Borders::ALL));
    frame.render_widget(statistics, sections[1]);

    let strategy_lines = vec![
        Line::from(format!("lower: {:.4}", detail.strategy.lower_price)),
        Line::from(format!("upper: {:.4}", detail.strategy.upper_price)),
        Line::from(format!("shape: {}", detail.strategy.shape_family)),
        Line::from(format!(
            "out of band policy: {}",
            detail.strategy.out_of_band_policy
        )),
    ];
    let strategy = Paragraph::new(strategy_lines)
        .block(Block::default().title("Strategy").borders(Borders::ALL));
    frame.render_widget(strategy, sections[2]);

    let execution_lines = execution_lines(
        &detail.execution,
        detail.market.mark_price,
        detail.market.index_price,
    );
    let execution = Paragraph::new(execution_lines)
        .block(Block::default().title("Execution").borders(Borders::ALL));
    frame.render_widget(execution, sections[3]);

    let command_lines: Vec<Line<'_>> = if detail.available_commands.is_empty() {
        vec![Line::from("No commands available")]
    } else {
        detail
            .available_commands
            .iter()
            .map(|command| Line::from(format_command(command)))
            .collect()
    };
    let commands = Paragraph::new(command_lines)
        .block(Block::default().title("Commands").borders(Borders::ALL));
    frame.render_widget(commands, sections[4]);

    let activity_lines: Vec<Line<'_>> = if detail.activity.is_empty() {
        vec![Line::from("No activity yet")]
    } else {
        detail
            .activity
            .iter()
            .map(|item| {
                let level = match item.level {
                    ActivityLevelView::Info => "info",
                    ActivityLevelView::Warn => "warn",
                    ActivityLevelView::Error => "error",
                };
                Line::from(format!("{} [{}] {}", item.ts, level, item.message))
            })
            .collect()
    };
    let activity = Paragraph::new(activity_lines)
        .block(Block::default().title("Activity").borders(Borders::ALL));
    frame.render_widget(activity, sections[5]);
}

fn execution_lines(
    execution: &GridExecutionView,
    mark_price: Option<f64>,
    index_price: Option<f64>,
) -> Vec<Line<'static>> {
    let state = match execution.state {
        ExecutionStateView::Open => "open",
        ExecutionStateView::Paused => "paused",
        ExecutionStateView::Closed => "closed",
    };

    let pending_order = execution
        .pending_order
        .as_ref()
        .map(|order| {
            format!(
                "{} {:.4} @ {:.4} ({})",
                order.side, order.quantity, order.price, order.status
            )
        })
        .unwrap_or_else(|| "none".to_string());

    vec![
        Line::from(format!("state: {state}")),
        Line::from(format!(
            "mark/index: {}/{}",
            format_optional_price(mark_price),
            format_optional_price(index_price)
        )),
        Line::from(format!("pending order: {pending_order}")),
    ]
}

fn format_optional_price(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_pnl(value: f64) -> String {
    format!("{value:+.2}")
}

fn format_command(command: &GridCommandView) -> String {
    let name = match command.command {
        GridCommandType::Pause => "pause",
        GridCommandType::Resume => "resume",
        GridCommandType::Terminate => "terminate",
        GridCommandType::Flatten => "flatten",
    };

    match (command.enabled, command.disabled_reason.as_deref()) {
        (true, _) => format!("{name}: enabled"),
        (false, Some(reason)) => format!("{name}: disabled - {reason}"),
        (false, None) => format!("{name}: disabled"),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, View};
    use crate::protocol::{GridCommandType, GridCommandView, GridDetailView};

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

    fn render_text(detail: GridDetailView) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::GridListResponse =
            serde_json::from_str(include_str!("../../tests/fixtures/grid_list_response.json"))
                .unwrap();
        let mut app = App::new(response.items);
        app.current_view = View::Instance;
        app.apply_grid_detail(detail);
        app.show_instance_for_selected();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        buffer_text(&terminal)
    }

    #[test]
    fn renders_grid_detail_execution_activity_and_commands() {
        let mut detail: GridDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/grid_detail_view.json"))
                .unwrap();
        detail.available_commands.push(GridCommandView {
            command: GridCommandType::Resume,
            enabled: false,
            disabled_reason: Some("grid is not paused".to_string()),
        });
        detail.available_commands.push(GridCommandView {
            command: GridCommandType::Flatten,
            enabled: false,
            disabled_reason: Some("no position to flatten".to_string()),
        });
        let text = render_text(detail);

        assert!(text.contains("Overview"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Statistics"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Activity"));
        assert!(text.contains("Commands"));
        assert!(text.contains("Total PnL"));
        assert!(text.contains("Realized PnL"));
        assert!(text.contains("+1245.30"));
        assert!(text.contains("+980.10"));
        assert!(text.contains("lower: 90.0000"));
        assert!(text.contains("upper: 110.0000"));
        assert!(text.contains("shape: linear"));
        assert!(text.contains("out of band policy: freeze"));
        assert!(text.contains("pause: enabled"));
        assert!(text.contains("terminate: disabled - risk review pending"));
        assert!(text.contains("resume: disabled - grid is not paused"));
        assert!(text.contains("flatten: disabled - no position to flatten"));
        assert!(!text.contains("client-1"));
    }

    #[test]
    fn renders_statistics_with_explicit_separator_for_large_pnl_values() {
        let mut detail: GridDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/grid_detail_view.json"))
                .unwrap();
        detail.statistics.total_pnl = -123456789.12;
        detail.statistics.realized_pnl = 987654321.99;

        let text = render_text(detail);

        assert!(text.contains("Total PnL | Realized PnL"));
        assert!(text.contains("-123456789.12 | +987654321.99"));
        assert!(!text.contains("-123456789.12+987654321.99"));
    }
}
