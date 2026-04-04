use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::protocol::{
    ActivityLevelView, ExecutionIntentView, ExecutionSlotPhaseView, ExecutionStateView,
    ExecutionStatusView, ReplacementGateView, TrackCommandType, TrackCommandView,
    TrackExecutionView,
};
use crate::signal::{exposure_signal, pnl_signal};
use crate::theme::Theme;
use crate::views::instance_layout::{
    DetailLayoutMode, resolve_detail_layout, resolve_trace_layout,
};

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(detail) = app.current_track_detail().or(app.current_track.as_ref()) else {
        let empty = Paragraph::new("No track detail loaded")
            .block(Block::default().title("Instance").borders(Borders::ALL));
        frame.render_widget(empty, area);
        return;
    };
    let sections = resolve_detail_layout(area);

    let status = Paragraph::new(status_lines(detail))
        .block(Block::default().title("Status").borders(Borders::ALL));
    frame.render_widget(status, sections.status);

    let overview = Paragraph::new(overview_lines(detail, sections.mode))
        .block(Block::default().title("Overview").borders(Borders::ALL));
    frame.render_widget(overview, sections.overview);

    let strategy = Paragraph::new(strategy_lines(detail, sections.mode))
        .block(Block::default().title("Strategy").borders(Borders::ALL));
    frame.render_widget(strategy, sections.strategy);

    let execution = Paragraph::new(execution_lines(
        &detail.execution,
        detail.market.mark_price,
        detail.market.index_price,
    ))
    .block(Block::default().title("Execution").borders(Borders::ALL));
    frame.render_widget(execution, sections.execution);

    if let Some(statistics_area) = sections.statistics {
        let statistics = Paragraph::new(statistics_lines(detail, sections.mode))
            .block(Block::default().title("Statistics").borders(Borders::ALL));
        frame.render_widget(statistics, statistics_area);
    }

    if let Some(trace_area) = sections.trace {
        render_trace(frame, trace_area, detail, app);
    }
}

fn status_lines(detail: &crate::protocol::TrackDetailView) -> Vec<Line<'static>> {
    let lifecycle = detail.status.lifecycle.status.to_string();
    let execution_state = match detail.execution.state {
        ExecutionStateView::Open => "open",
        ExecutionStateView::Paused => "paused",
        ExecutionStateView::Closed => "closed",
    };
    let mut lines = Vec::new();

    if matches!(
        detail.execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        let reason_summary = attention_summary(&detail.execution.attention_reasons);
        lines.push(Line::from(Span::styled(
            format!(
                "! ATTENTION REQUIRED | {reason_summary} | gap {:.4} | age {} ms",
                detail.execution.inventory_gap, detail.execution.gap_age_ms
            ),
            Theme::execution_attention(),
        )));
    } else {
        lines.push(Line::from(format!(
            "{lifecycle} | {execution_state} | gap {:.4} | {} active slot",
            detail.execution.inventory_gap, detail.execution.active_slot_count
        )));
    }

    lines.push(Line::from(format!(
        "{} / {} / {} | updated {}",
        detail.identity.id,
        detail.identity.instrument.symbol,
        detail.identity.instrument.venue,
        detail.status.lifecycle.updated_at
    )));

    let commands = status_command_hint(&detail.available_commands);
    if !commands.is_empty() {
        lines.push(Line::from(commands));
    }

    lines
}

fn overview_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(format!(
            "id/symbol/venue/lifecycle: {} / {} / {} / {}",
            detail.identity.id,
            detail.identity.instrument.symbol,
            detail.identity.instrument.venue,
            detail.status.lifecycle.status
        )),
        Line::from(format!(
            "reference/mark/index: {} / {} / {}",
            format_optional_price(detail.status.reference_price),
            format_optional_price(detail.market.mark_price),
            format_optional_price(detail.market.index_price)
        )),
        format_exposure_line(
            detail.status.reference_price,
            detail.position.current_exposure,
            detail.position.desired_exposure,
        ),
    ];

    if matches!(mode, DetailLayoutMode::Minimal) {
        lines.push(Line::from(format!(
            "stats: total {} | realized {}",
            pnl_signal(detail.statistics.total_pnl).text,
            pnl_signal(detail.statistics.realized_pnl).text
        )));
    }

    lines
}

fn strategy_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Compact | DetailLayoutMode::Minimal) {
        vec![
            Line::from(format!(
                "band: {:.4} -> {:.4} | shape: {} | out of band: {}",
                detail.strategy.lower_price,
                detail.strategy.upper_price,
                detail.strategy.shape_family,
                detail.strategy.out_of_band_policy
            )),
            Line::from(format!(
                "long/short units: {:.4} / {:.4}",
                detail.strategy.long_exposure_units, detail.strategy.short_exposure_units
            )),
            Line::from(format!(
                "notional per unit: {:.4} | min rebalance units: {:.4}",
                detail.strategy.notional_per_unit, detail.strategy.min_rebalance_units
            )),
        ]
    } else {
        vec![
            Line::from(format!(
                "lower/upper: {:.4} / {:.4}",
                detail.strategy.lower_price, detail.strategy.upper_price
            )),
            Line::from(format!(
                "long/short units: {:.4} / {:.4}",
                detail.strategy.long_exposure_units, detail.strategy.short_exposure_units
            )),
            Line::from(format!(
                "notional per unit: {:.4} | min rebalance units: {:.4}",
                detail.strategy.notional_per_unit, detail.strategy.min_rebalance_units
            )),
            Line::from(format!(
                "shape: {} | out of band policy: {}",
                detail.strategy.shape_family, detail.strategy.out_of_band_policy
            )),
        ]
    }
}

fn statistics_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Compact) {
        vec![Line::from(format!(
            "stats: total {} | realized {} | max gap {:.4} | age {} ms",
            pnl_signal(detail.statistics.total_pnl).text,
            pnl_signal(detail.statistics.realized_pnl).text,
            detail.statistics.max_inventory_gap_abs,
            detail.statistics.max_gap_age_ms
        ))]
    } else {
        vec![
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
        ]
    }
}

fn render_trace(
    frame: &mut Frame<'_>,
    area: Rect,
    detail: &crate::protocol::TrackDetailView,
    app: &App,
) {
    let block = Block::default().title("Trace").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let trace_layout = resolve_trace_layout(inner, app.debug_diagnostics_enabled());

    let activity = Paragraph::new(trace_activity_lines(
        detail,
        trace_layout.activity.max_entries,
    ));
    frame.render_widget(activity, trace_layout.activity.area);

    if let Some(diagnostics_area) = trace_layout.diagnostics {
        let diagnostics =
            Paragraph::new(trace_diagnostics_lines(app, diagnostics_area.max_entries));
        frame.render_widget(diagnostics, diagnostics_area.area);
    }
}

fn trace_activity_lines(
    detail: &crate::protocol::TrackDetailView,
    max_entries: usize,
) -> Vec<Line<'static>> {
    let entries = if detail.activity.is_empty() {
        vec![Line::from("No activity yet")]
    } else {
        detail
            .activity
            .iter()
            .map(|item| format_trace_item_line(&item.ts, item.level, &item.message))
            .collect()
    };

    trim_trace_section("Activity", entries, max_entries)
}

fn trace_diagnostics_lines(app: &App, max_entries: usize) -> Vec<Line<'static>> {
    let entries = if let Some(diagnostics) = app.current_track_diagnostics() {
        if diagnostics.items.is_empty() {
            vec![Line::from("No diagnostics yet")]
        } else {
            diagnostics
                .items
                .iter()
                .map(|item| format_trace_item_line(&item.ts, item.level, &item.message))
                .collect()
        }
    } else {
        vec![Line::from("No diagnostics loaded")]
    };

    trim_trace_section("Diagnostics", entries, max_entries)
}

fn trim_trace_section(
    title: &'static str,
    mut entries: Vec<Line<'static>>,
    max_entries: usize,
) -> Vec<Line<'static>> {
    if max_entries == 0 {
        return vec![Line::from(title)];
    }

    if entries.len() > max_entries {
        let keep_from = entries.len() - max_entries;
        entries = entries.split_off(keep_from);
    }

    let mut lines = vec![Line::from(title)];
    lines.extend(entries);
    lines
}

fn format_trace_item_line(ts: &str, level: ActivityLevelView, message: &str) -> Line<'static> {
    let level = match level {
        ActivityLevelView::Info => "info",
        ActivityLevelView::Warn => "warn",
        ActivityLevelView::Error => "error",
    };

    Line::from(format!(
        "{} [{}] {}",
        format_activity_timestamp(ts),
        level,
        message
    ))
}

fn attention_summary(attention_reasons: &[String]) -> String {
    match attention_reasons {
        [] => "unresolved execution anomaly".to_string(),
        [reason] => reason.clone(),
        reasons => format!("{} reasons", reasons.len()),
    }
}

fn execution_lines(
    execution: &TrackExecutionView,
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

fn format_exposure_line(
    reference_price: Option<f64>,
    current: f64,
    target: Option<f64>,
) -> Line<'static> {
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
        Span::raw(format!(
            " | {max_inventory_gap_abs:.4} | {max_gap_age_ms} ms"
        )),
    ])
}

fn status_command_hint(commands: &[TrackCommandView]) -> String {
    let hints = commands
        .iter()
        .filter(|command| command.enabled)
        .filter_map(|command| match command.command {
            TrackCommandType::Pause => Some("p pause".to_string()),
            TrackCommandType::Resume => Some("r resume".to_string()),
            TrackCommandType::Terminate => Some("t terminate".to_string()),
            TrackCommandType::Flatten => Some("f flatten".to_string()),
        })
        .collect::<Vec<_>>();

    if hints.is_empty() {
        String::new()
    } else {
        format!("commands: {}", hints.join(" | "))
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use crate::app::{App, View};
    use crate::protocol::{
        ExecutionStatusView, TrackCommandType, TrackCommandView, TrackDetailView,
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

    fn render_text_with_debug(
        detail: TrackDetailView,
        diagnostics: Option<TrackDiagnosticsView>,
        width: u16,
        height: u16,
    ) -> String {
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

        if let Some(diagnostics) = diagnostics {
            app.toggle_debug_diagnostics();
            app.apply_track_diagnostics(diagnostics);
        }

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        buffer_text(&terminal)
    }

    fn diagnostics_view() -> TrackDiagnosticsView {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/track_diagnostics_view.json"
        ))
        .unwrap()
    }

    #[test]
    fn renders_redesigned_detail_sections_and_status_commands() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text(detail);

        assert!(text.contains("Status"));
        assert!(text.contains("Overview"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Statistics"));
        assert!(text.contains("Trace"));
        assert!(!text.contains("Commands"));
        assert!(text.contains("commands: p pause"));
        assert!(text.contains("min rebalance units"));
    }

    #[test]
    fn renders_trace_panel_with_diagnostics_in_debug_view() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let default_text = render_text_with_debug(detail.clone(), None, 100, 36);
        assert!(default_text.contains("Trace"));
        assert!(default_text.contains("Activity"));
        assert!(!default_text.contains("Diagnostics"));

        let debug_text = render_text_with_debug(detail, Some(diagnostics_view()), 100, 36);
        assert!(debug_text.contains("Trace"));
        assert!(debug_text.contains("Diagnostics"));
        assert!(debug_text.contains("desired exposure 3.5000 -> 4.0000"));
    }

    #[test]
    fn renders_trace_placeholder_when_debug_is_enabled_without_loaded_diagnostics() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        let backend = TestBackend::new(100, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        let response: crate::protocol::TrackListResponse = serde_json::from_str(include_str!(
            "../../tests/fixtures/track_list_response.json"
        ))
        .unwrap();
        let mut app = App::new(response.items);
        app.current_view = View::Instance;
        app.apply_track_detail(detail);
        app.show_instance_for_selected();
        app.toggle_debug_diagnostics();

        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("Diagnostics"));
        assert!(text.contains("No diagnostics loaded"));
    }

    #[test]
    fn renders_trace_with_recent_activity_and_diagnostics_when_space_is_limited() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.activity = (1..=4)
            .map(|index| {
                let mut item = detail.activity[0].clone();
                item.message = format!("activity {index}");
                item
            })
            .collect();

        let debug_text = render_text_with_debug(detail, Some(diagnostics_view()), 100, 36);

        assert!(debug_text.contains("Activity"));
        assert!(debug_text.contains("activity 4"));
        assert!(!debug_text.contains("activity 1"));
        assert!(debug_text.contains("Diagnostics"));
        assert!(debug_text.contains("desired exposure 3.5000 -> 4.0000"));
    }

    #[test]
    fn renders_compact_detail_layout_when_height_is_limited() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text_with_size(detail, 100, 24);

        assert!(text.contains("Status"));
        assert!(text.contains("Trace"));
        assert!(text.contains("stats:"));
    }

    #[test]
    fn renders_minimal_detail_layout_when_height_is_tight() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text_with_size(detail, 100, 16);

        assert!(text.contains("Status"));
        assert!(text.contains("Overview"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Execution"));
        assert!(!text.contains("Trace"));
        assert!(!text.contains("Statistics"));
    }

    #[test]
    fn renders_track_detail_execution_activity_and_commands() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.available_commands.push(TrackCommandView {
            command: TrackCommandType::Resume,
            enabled: false,
            disabled_reason: Some("track is not paused".to_string()),
        });
        detail.available_commands.push(TrackCommandView {
            command: TrackCommandType::Flatten,
            enabled: false,
            disabled_reason: Some("no position to flatten".to_string()),
        });
        let text = render_text(detail);

        assert!(text.contains("Overview"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Statistics"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Trace"));
        assert!(text.contains("Activity"));
        assert!(!text.contains("Commands"));
        assert!(text.contains("commands: p pause"));
        assert!(text.contains("Total PnL"));
        assert!(text.contains("↑ +1245.30"));
        assert!(text.contains("↑ +980.10"));
        assert!(text.contains("stats since: 2026-03-26T09:45:00Z"));
        assert!(text.contains("lower/upper: 90.0000 / 110.0000"));
        assert!(text.contains("long/short units: 8.0000 / 8.0000"));
        assert!(text.contains("notional per unit: 375.0000 | min rebalance units: 0.5000"));
        assert!(text.contains("shape: linear | out of band policy: freeze"));
        assert!(text.contains("reference/exposure: 101.2500 / 3.5000 → 4.0000 [↑ +0.5000]"));
        assert!(text.contains("execution status: normal"));
        assert!(text.contains("inventory gap / age: 0.5000 / 60000 ms"));
        assert!(text.contains("active slots: 1"));
        assert!(text.contains("inventory_core opening increase_inventory buy 0.0100 @ 100.5000"));
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
        assert!(!default_text.contains("desired exposure 3.5000 -> 4.0000"));

        app.toggle_debug_diagnostics();
        app.apply_track_diagnostics(diagnostics_view());
        terminal
            .draw(|frame| render(frame, frame.area(), &app))
            .unwrap();
        let debug_text = buffer_text(&terminal);
        assert!(debug_text.contains("Diagnostics"));
        assert!(debug_text.contains("desired exposure 3.5000 -> 4.0000"));
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

        assert!(text.contains(
            "! ATTENTION REQUIRED | recovery anomaly: unknown_live_order | gap 0.5000 | age 60000 ms"
        ));
        assert!(text.contains("recovery anomaly: unknown_live_order"));
        assert!(
            background_colors_for_substring(&terminal, "! ATTENTION REQUIRED")
                .iter()
                .any(|bg| *bg != Color::Reset)
        );
    }

    #[test]
    fn renders_attention_summary_without_hiding_gap_and_age() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.execution.execution_status = ExecutionStatusView::AttentionRequired;
        detail.execution.attention_reasons = vec![
            "recovery anomaly: duplicate_live_orders".to_string(),
            "market data stale".to_string(),
            "insufficient account margin".to_string(),
        ];

        let text = render_text_with_size(detail, 100, 36);

        assert!(text.contains("! ATTENTION REQUIRED | 3 reasons"));
        assert!(text.contains("gap 0.5000 | age 60000 ms"));
        assert!(text.contains("alerts: recovery anomaly: duplicate_live_orders | market data stale | insufficient account margin"));
    }
}
