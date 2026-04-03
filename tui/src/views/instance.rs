use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::protocol::{
    ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStateView,
    ExecutionStatusView, GridCommandType, GridCommandView, GridExecutionView, ReplacementGateView,
};
use crate::signal::{exposure_signal, pnl_signal};
use crate::theme::Theme;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(detail) = app.current_track_detail().or(app.current_track.as_ref()) else {
        let empty = Paragraph::new("No track detail loaded")
            .block(Block::default().title("Instance").borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };

    let execution_height = if matches!(
        detail.execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        11
    } else {
        9
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if app.debug_diagnostics_enabled() {
            vec![
                Constraint::Length(6),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(execution_height),
                Constraint::Length(6),
                Constraint::Min(0),
                Constraint::Length(5),
            ]
        } else {
            vec![
                Constraint::Length(6),
                Constraint::Length(5),
                Constraint::Length(6),
                Constraint::Length(execution_height),
                Constraint::Length(6),
                Constraint::Min(0),
            ]
        })
        .split(area);

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
        format_exposure_line(
            detail.status.reference_price,
            detail.position.current_exposure,
            detail.position.target_exposure,
        ),
    ];
    let summary = Paragraph::new(summary_lines)
        .block(Block::default().title("Overview").borders(Borders::ALL));
    frame.render_widget(summary, sections[0]);

    let statistics_lines = vec![
        Line::from("Total PnL | Realized PnL | Max Gap | Max Gap Age"),
        format_statistics_line(
            detail.statistics.total_pnl,
            detail.statistics.realized_pnl,
            detail.statistics.max_inventory_gap_abs,
            detail.statistics.max_gap_age_ms,
        ),
        Line::from(format!(
            "stats since: {}",
            detail
                .statistics
                .stats_started_at
                .clone()
                .unwrap_or_else(|| "-".to_string())
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
                Line::from(format!(
                    "{} [{}] {}",
                    format_activity_timestamp(&item.ts),
                    level,
                    item.message
                ))
            })
            .collect()
    };
    let activity = Paragraph::new(activity_lines)
        .block(Block::default().title("Activity").borders(Borders::ALL));
    frame.render_widget(activity, sections[5]);

    if app.debug_diagnostics_enabled() {
        let diagnostics_lines: Vec<Line<'_>> =
            if let Some(diagnostics) = app.current_track_diagnostics() {
                if diagnostics.items.is_empty() {
                    vec![Line::from("No diagnostics yet")]
                } else {
                    diagnostics
                        .items
                        .iter()
                        .map(|item| {
                            let level = match item.level {
                                ActivityLevelView::Info => "info",
                                ActivityLevelView::Warn => "warn",
                                ActivityLevelView::Error => "error",
                            };
                            Line::from(format!(
                                "{} [{}] {}",
                                format_activity_timestamp(&item.ts),
                                level,
                                item.message
                            ))
                        })
                        .collect()
                }
            } else {
                vec![Line::from("No diagnostics loaded")]
            };
        let diagnostics = Paragraph::new(diagnostics_lines)
            .block(Block::default().title("Diagnostics").borders(Borders::ALL));
        frame.render_widget(diagnostics, sections[6]);
    }
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
    let execution_status = match execution.execution_status {
        ExecutionStatusView::Normal => "normal",
        ExecutionStatusView::AttentionRequired => "attention_required",
    };
    let slots = if execution.slots.is_empty() {
        "none".to_string()
    } else {
        execution
            .slots
            .iter()
            .map(|slot| {
                let order = slot
                    .order
                    .as_ref()
                    .map(|order| {
                        format!("{} {:.4} @ {:.4}", order.side, order.quantity, order.price)
                    })
                    .unwrap_or_else(|| "no order".to_string());
                format!(
                    "{} {} {} {}",
                    slot.label,
                    format_slot_phase(slot.phase),
                    format_slot_intent(slot.intent),
                    order
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let replacement_gate = execution
        .replacement_gate
        .as_ref()
        .map(format_replacement_gate)
        .unwrap_or_else(|| "-".to_string());

    let mut lines = Vec::new();

    if matches!(
        execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        lines.push(Line::from(Span::styled(
            "! ATTENTION REQUIRED",
            Theme::execution_attention(),
        )));
        if execution.attention_reasons.is_empty() {
            lines.push(Line::from("alerts: unresolved execution anomaly"));
        } else {
            lines.push(Line::from(format!(
                "alerts: {}",
                execution.attention_reasons.join(" | ")
            )));
        }
    }

    lines.extend([
        Line::from(format!("state: {state}")),
        Line::from(format!("execution status: {execution_status}")),
        Line::from(format!(
            "mark/index: {}/{}",
            format_optional_price(mark_price),
            format_optional_price(index_price)
        )),
        Line::from(format!(
            "inventory gap / age: {:.4} / {} ms",
            execution.inventory_gap, execution.gap_age_ms
        )),
        Line::from(format!("active slots: {}", execution.active_slot_count)),
        Line::from(format!("slots: {slots}")),
        Line::from(format!("replacement gate: {replacement_gate}")),
    ]);

    lines
}

fn format_slot_phase(value: ExecutionSlotPhaseView) -> &'static str {
    match value {
        ExecutionSlotPhaseView::Opening => "opening",
        ExecutionSlotPhaseView::Working => "working",
    }
}

fn format_slot_intent(value: ExecutionIntentView) -> &'static str {
    match value {
        ExecutionIntentView::IncreaseInventory => "increase_inventory",
        ExecutionIntentView::DecreaseInventory => "decrease_inventory",
    }
}

fn format_replacement_gate(value: &ReplacementGateView) -> String {
    match value {
        ReplacementGateView::RoundedMatch => "rounded match".to_string(),
        ReplacementGateView::ImprovementBelowThreshold {
            improvement_bps,
            threshold_bps,
        } => format!("{improvement_bps:.1} bps < {threshold_bps:.1} bps"),
    }
}

fn format_optional_price(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string())
}

fn format_exposure_line(reference_price: Option<f64>, current: f64, target: Option<f64>) -> Line<'static> {
    let signal = exposure_signal(current, target);
    let reference_text = reference_price
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string());
    let target_text = target
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string());

    Line::from(vec![
        Span::raw(format!("reference/exposure: {reference_text} / ")),
        Span::raw(format!("{current:.4} → {target_text} ")),
        Span::styled(format!("[{}]", signal.text), signal.style),
    ])
}

fn format_statistics_line(
    total_pnl: f64,
    realized_pnl: f64,
    max_inventory_gap_abs: f64,
    max_gap_age_ms: i64,
) -> Line<'static> {
    let total = pnl_signal(total_pnl);
    let realized = pnl_signal(realized_pnl);

    Line::from(vec![
        Span::styled(total.text, total.style),
        Span::raw(" | "),
        Span::styled(realized.text, realized.style),
        Span::raw(format!(" | {max_inventory_gap_abs:.4} | {max_gap_age_ms} ms")),
    ])
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

fn format_activity_timestamp(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|value| {
            value
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|_| ts.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Local};
    use ratatui::style::Color;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, View};
    use crate::protocol::{
        ExecutionStatusView, GridCommandType, GridCommandView, TrackDetailView,
        TrackDiagnosticsView,
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

    fn background_colors_for_substring(terminal: &Terminal<TestBackend>, needle: &str) -> Vec<Color> {
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

    fn render_terminal(detail: TrackDetailView, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut app = App::new(response.items);
        app.current_view = View::Instance;
        app.apply_track_detail(detail);
        app.show_instance_for_selected();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        terminal
    }

    fn render_text_with_size(detail: TrackDetailView, width: u16, height: u16) -> String {
        let terminal = render_terminal(detail, width, height);
        buffer_text(&terminal)
    }

    fn render_text(detail: TrackDetailView) -> String {
        render_text_with_size(detail, 100, 36)
    }

    fn diagnostics_view() -> TrackDiagnosticsView {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/track_diagnostics_view.json"
        ))
        .unwrap()
    }

    #[test]
    fn renders_grid_detail_execution_activity_and_commands() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
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
        assert!(text.contains("Max Gap"));
        assert!(text.contains("Max Gap Age"));
        assert!(text.contains("↑ +1245.30"));
        assert!(text.contains("↑ +980.10"));
        assert!(text.contains("stats since: 2026-03-26T09:45:00Z"));
        assert!(text.contains("lower: 90.0000"));
        assert!(text.contains("upper: 110.0000"));
        assert!(text.contains("shape: linear"));
        assert!(text.contains("out of band policy: freeze"));
        assert!(text.contains("reference/exposure: 101.2500 / 3.5000 → 4.0000 [↑ +0.5000]"));
        assert!(text.contains("execution status: normal"));
        assert!(text.contains("inventory gap / age: 0.5000 / 60000 ms"));
        assert!(text.contains("active slots: 1"));
        assert!(text.contains("inventory_core opening increase_inventory buy 0.0100 @ 100.5000"));
        assert!(text.contains("pause: enabled"));
        assert!(text.contains("terminate: disabled - risk review pending"));
        assert!(text.contains("resume: disabled - grid is not paused"));
        assert!(text.contains("flatten: disabled - no position to flatten"));
        assert!(text.contains("replacement gate"));
        assert!(text.contains("9.0 bps < 13.0 bps"));
        assert!(!text.contains("Diagnostics"));
        assert!(!text.contains("client-1"));
    }

    #[test]
    fn diagnostics_panel_is_hidden_by_default_and_visible_in_debug_view() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut app = App::new(response.items);
        app.current_view = View::Instance;
        app.apply_track_detail(detail.clone());
        app.show_instance_for_selected();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let default_text = buffer_text(&terminal);
        assert!(!default_text.contains("Diagnostics"));
        assert!(!default_text.contains("target exposure 3.5000 -> 4.0000"));

        app.toggle_debug_diagnostics();
        app.apply_track_diagnostics(diagnostics_view());
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let debug_text = buffer_text(&terminal);
        assert!(debug_text.contains("Diagnostics"));
        assert!(debug_text.contains("target exposure 3.5000 -> 4.0000"));
    }

    #[test]
    fn renders_statistics_with_explicit_separator_for_large_pnl_values() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.statistics.total_pnl = -123456789.12;
        detail.statistics.realized_pnl = 987654321.99;

        let text = render_text(detail);

        assert!(text.contains("Total PnL | Realized PnL"));
        assert!(text.contains("↓ -123456789.12 | ↑ +987654321.99"));
        assert!(!text.contains("-123456789.12+987654321.99"));
    }

    #[test]
    fn renders_activity_timestamp_in_local_time() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text(detail.clone());
        let original_ts = detail.activity[0].ts.clone();
        let expected_local = DateTime::parse_from_rfc3339(&original_ts)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();

        assert!(text.contains(&expected_local));
        assert!(!text.contains(&original_ts));
    }

    #[test]
    fn keeps_original_activity_timestamp_when_parsing_fails() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.activity[0].ts = "not-a-timestamp".to_string();

        let text = render_text(detail);

        assert!(text.contains("not-a-timestamp"));
    }

    #[test]
    fn renders_attention_required_block_with_reason() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.execution.execution_status = ExecutionStatusView::AttentionRequired;
        detail.execution.attention_reasons =
            vec!["recovery anomaly: unknown_live_order".to_string()];

        let terminal = render_terminal(detail, 100, 36);
        let text = buffer_text(&terminal);

        assert!(text.contains("! ATTENTION REQUIRED"));
        assert!(text.contains("recovery anomaly: unknown_live_order"));
        assert!(
            background_colors_for_substring(&terminal, "! ATTENTION REQUIRED")
                .iter()
                .any(|bg| *bg != Color::Reset)
        );
    }
}
