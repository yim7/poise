use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::exposure_presentation::instance_exposure_annotation;
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

    let track = Paragraph::new(track_lines(detail, sections.mode))
        .block(Block::default().title("Track").borders(Borders::ALL));
    frame.render_widget(track, sections.track);

    if let Some(pnl_area) = sections.pnl {
        let pnl = Paragraph::new(pnl_lines(detail))
            .block(Block::default().title("PnL").borders(Borders::ALL));
        frame.render_widget(pnl, pnl_area);
    }

    if let Some(execution_stats_area) = sections.execution_stats {
        let execution_stats = Paragraph::new(execution_stats_lines(detail, sections.mode)).block(
            Block::default()
                .title("Execution Stats")
                .borders(Borders::ALL),
        );
        frame.render_widget(execution_stats, execution_stats_area);
    }

    let market = Paragraph::new(market_lines(detail, sections.mode))
        .block(Block::default().title("Market").borders(Borders::ALL));
    frame.render_widget(market, sections.market);

    if let Some(strategy_area) = sections.strategy {
        let strategy = Paragraph::new(strategy_lines(detail, sections.mode))
            .block(Block::default().title("Strategy").borders(Borders::ALL));
        frame.render_widget(strategy, strategy_area);
    }

    let execution = Paragraph::new(execution_lines(&detail.execution, sections.mode))
        .block(Block::default().title("Execution").borders(Borders::ALL));
    frame.render_widget(execution, sections.execution);

    if let Some(trace_area) = sections.trace {
        render_trace(frame, trace_area, detail, app);
    }
}

fn track_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Minimal) {
        return vec![minimal_track_line(detail)];
    }

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
        let slot_count = format_slot_count(detail.execution.active_slot_count);
        lines.push(Line::from(Span::styled(
            format!(
                "{lifecycle} | {execution_state} | ATTENTION REQUIRED | {reason_summary} | gap {:.4} | age {} ms | {slot_count}",
                detail.execution.inventory_gap,
                detail.execution.gap_age_ms,
            ),
            Theme::execution_attention(),
        )));
    } else {
        lines.push(Line::from(format!(
            "{lifecycle} | {execution_state} | gap {:.4} | age {} ms | {}",
            detail.execution.inventory_gap,
            detail.execution.gap_age_ms,
            format_slot_count(detail.execution.active_slot_count)
        )));
    }

    lines.push(Line::from(format!(
        "{} | updated {}",
        detail.identity.instrument.venue, detail.status.lifecycle.updated_at
    )));

    let commands = status_command_hint(&detail.available_commands);
    if !commands.is_empty() {
        lines.push(Line::from(commands));
    }

    lines
}

fn minimal_track_line(detail: &crate::protocol::TrackDetailView) -> Line<'static> {
    let lifecycle = detail.status.lifecycle.status.to_string();
    let execution_state = match detail.execution.state {
        ExecutionStateView::Open => "open",
        ExecutionStateView::Paused => "paused",
        ExecutionStateView::Closed => "closed",
    };

    if matches!(
        detail.execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        Line::from(Span::styled(
            format!(
                "{lifecycle} | {execution_state} | ATTENTION | gap {:.4} | {}",
                detail.execution.inventory_gap,
                format_slot_count(detail.execution.active_slot_count)
            ),
            Theme::execution_attention(),
        ))
    } else {
        Line::from(format!(
            "{lifecycle} | {execution_state} | gap {:.4} | {}",
            detail.execution.inventory_gap,
            format_slot_count(detail.execution.active_slot_count)
        ))
    }
}

fn market_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Minimal) {
        return vec![format_exposure_line(
            detail.status.reference_price,
            detail.position.current_exposure,
            detail.position.desired_exposure,
        )];
    }

    vec![
        Line::from(format!(
            "prices: ref {} | mark {} | index {}",
            format_optional_price(detail.status.reference_price),
            format_optional_price(detail.market.mark_price),
            format_optional_price(detail.market.index_price)
        )),
        format_exposure_line(
            detail.status.reference_price,
            detail.position.current_exposure,
            detail.position.desired_exposure,
        ),
    ]
}

fn strategy_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    if matches!(mode, DetailLayoutMode::Minimal) {
        vec![Line::from(format!(
            "band {:.4}->{:.4} | shape {} | {}",
            detail.strategy.lower_price,
            detail.strategy.upper_price,
            detail.strategy.shape_family,
            detail.strategy.out_of_band_policy
        ))]
    } else if matches!(mode, DetailLayoutMode::Compact) {
        vec![
            Line::from(format!(
                "band: {:.4} -> {:.4} | shape: {} | out of band: {}",
                detail.strategy.lower_price,
                detail.strategy.upper_price,
                detail.strategy.shape_family,
                detail.strategy.out_of_band_policy
            )),
            Line::from(format!(
                "units {:.4}/{:.4} | notional {:.4} | min {:.4}",
                detail.strategy.long_exposure_units,
                detail.strategy.short_exposure_units,
                detail.strategy.notional_per_unit,
                detail.strategy.min_rebalance_units
            )),
        ]
    } else {
        vec![
            Line::from(format!(
                "lower/upper: {:.4} / {:.4}",
                detail.strategy.lower_price, detail.strategy.upper_price
            )),
            Line::from(format!(
                "units {:.4}/{:.4} | notional {:.4}",
                detail.strategy.long_exposure_units,
                detail.strategy.short_exposure_units,
                detail.strategy.notional_per_unit
            )),
            Line::from(format!(
                "min rebalance {:.4} | shape {} | out of band {}",
                detail.strategy.min_rebalance_units,
                detail.strategy.shape_family,
                detail.strategy.out_of_band_policy
            )),
        ]
    }
}

fn pnl_lines(detail: &crate::protocol::TrackDetailView) -> Vec<Line<'static>> {
    vec![format_pnl_summary_line(
        detail.pnl.total_pnl,
        detail.pnl.realized_pnl,
        detail.pnl.unrealized_pnl,
    )]
}

fn execution_stats_lines(
    detail: &crate::protocol::TrackDetailView,
    mode: DetailLayoutMode,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "max gap {:.4} | age {} ms",
        detail.execution_stats.max_inventory_gap_abs, detail.execution_stats.max_gap_age_ms
    ))];

    if matches!(mode, DetailLayoutMode::Standard) {
        lines.push(Line::from(format!(
            "execution stats since: {}",
            detail
                .execution_stats
                .stats_started_at
                .clone()
                .unwrap_or_else(|| "-".to_string())
        )));
    }

    lines
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

fn execution_lines(execution: &TrackExecutionView, mode: DetailLayoutMode) -> Vec<Line<'static>> {
    let slot_details = execution
        .slots
        .iter()
        .map(format_slot_detail)
        .collect::<Vec<_>>();
    let replacement_gate = execution
        .replacement_gate
        .as_ref()
        .map(format_replacement_gate);

    if matches!(mode, DetailLayoutMode::Minimal) {
        return minimal_execution_lines(execution, &slot_details, replacement_gate.as_deref());
    }

    let mut lines = Vec::new();

    if matches!(
        execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        if execution.attention_reasons.is_empty() {
            lines.push(Line::from("alerts: unresolved execution anomaly"));
        } else {
            lines.push(Line::from(format!(
                "alerts: {}",
                execution.attention_reasons.join(" | ")
            )));
        }
    }

    if lines.is_empty() && slot_details.is_empty() && replacement_gate.is_none() {
        return vec![Line::from("no working slots")];
    }

    if matches!(mode, DetailLayoutMode::Compact) {
        let execution_summary =
            format_compact_execution_summary(execution, replacement_gate.as_deref());
        if let Some(summary) = execution_summary {
            lines.push(Line::from(summary));
        }
    } else {
        if !slot_details.is_empty() {
            lines.push(Line::from(format!("slots: {}", slot_details.join(" | "))));
        } else if !lines.is_empty() {
            lines.push(Line::from("slots: none"));
        }

        if let Some(replacement_gate) = replacement_gate {
            lines.push(Line::from(format!("replacement gate: {replacement_gate}")));
        }
    }

    lines
}

fn minimal_execution_lines(
    execution: &TrackExecutionView,
    slot_details: &[String],
    replacement_gate: Option<&str>,
) -> Vec<Line<'static>> {
    if matches!(
        execution.execution_status,
        ExecutionStatusView::AttentionRequired
    ) {
        return vec![Line::from(format!(
            "alerts: {}",
            attention_summary(&execution.attention_reasons)
        ))];
    }

    if let Some(summary) = format_compact_execution_summary(execution, replacement_gate) {
        return vec![Line::from(summary)];
    }

    if slot_details.is_empty() && replacement_gate.is_none() {
        return vec![Line::from("no working slots")];
    }

    vec![Line::from("execution detail unavailable")]
}

fn format_compact_execution_summary(
    execution: &TrackExecutionView,
    replacement_gate: Option<&str>,
) -> Option<String> {
    let slots = compact_slot_summary(execution);

    match (slots.as_deref(), replacement_gate) {
        (None, None) => None,
        (Some(slots), None) => Some(slots.to_string()),
        (None, Some(replacement_gate)) => Some(format!("gate: {replacement_gate}")),
        (Some(slots), Some(replacement_gate)) => {
            Some(format!("{slots} | gate: {replacement_gate}"))
        }
    }
}

fn compact_slot_summary(execution: &TrackExecutionView) -> Option<String> {
    match execution.slots.as_slice() {
        [] => None,
        [slot] => Some(format!("slot: {}", compact_slot_label(slot))),
        slots => Some(format!(
            "slots {} | {}",
            slots.len(),
            compact_slot_label(&slots[0])
        )),
    }
}

fn format_slot_count(count: u32) -> String {
    format!("slots {count}")
}

fn format_slot_detail(slot: &poise_protocol::ExecutionSlotView) -> String {
    let order = slot
        .order
        .as_ref()
        .map(|order| format!("{} {:.4} @ {:.4}", order.side, order.quantity, order.price))
        .unwrap_or_else(|| "no order".to_string());
    format!(
        "{} {} {} {}",
        slot.label,
        format_slot_phase(slot.phase),
        format_slot_intent(slot.intent),
        order
    )
}

fn compact_slot_label(slot: &poise_protocol::ExecutionSlotView) -> String {
    let order = slot
        .order
        .as_ref()
        .map(|order| format!("{} {:.4} @ {:.4}", order.side, order.quantity, order.price))
        .unwrap_or_else(|| "no order".to_string());
    format!("{} {order}", slot.label)
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
    _reference_price: Option<f64>,
    current: f64,
    target: Option<f64>,
) -> Line<'static> {
    let signal = exposure_signal(current, target);
    let annotation = instance_exposure_annotation(signal);
    let target_text = target
        .map(|value| format!("{value:.4}"))
        .unwrap_or_else(|| "-".to_string());

    Line::from(vec![
        Span::raw(format!("exposure: {current:.4} → {target_text} ")),
        Span::styled(annotation.text, annotation.style),
    ])
}

fn format_pnl_summary_line(
    total_pnl: f64,
    realized_pnl: f64,
    unrealized_pnl: f64,
) -> Line<'static> {
    let total = pnl_signal(total_pnl);
    let unrealized = pnl_signal(unrealized_pnl);
    let realized = pnl_signal(realized_pnl);

    Line::from(vec![
        Span::raw("total "),
        Span::styled(total.text, total.style),
        Span::raw(" | unrealized "),
        Span::styled(unrealized.text, unrealized.style),
        Span::raw(" | realized cumulative "),
        Span::styled(realized.text, realized.style),
    ])
}

fn status_command_hint(commands: &[TrackCommandView]) -> String {
    let hints = commands
        .iter()
        .filter(|command| command.enabled)
        .map(|command| match command.command {
            TrackCommandType::Pause => "p pause".to_string(),
            TrackCommandType::Resume => "r resume".to_string(),
            TrackCommandType::Terminate => "t terminate".to_string(),
            TrackCommandType::Flatten => "f flatten".to_string(),
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
    use crate::app::{App, View};
    use crate::protocol::{
        ExecutionStatusView, TrackCommandType, TrackCommandView, TrackDetailView,
        TrackDiagnosticsView,
    };
    use chrono::{DateTime, Local};
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

        assert!(text.contains("Track"));
        assert!(text.contains("PnL"));
        assert!(text.contains("Execution Stats"));
        assert!(text.contains("Market"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Trace"));
        assert!(!text.contains("Commands"));
        assert!(text.contains("commands: p pause"));
        assert!(text.contains("min"));
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
        assert!(debug_text.contains("activity 1"));
        assert!(debug_text.contains("Diagnostics"));
        assert!(debug_text.contains("desired exposure 3.5000 -> 4.0000"));
    }

    #[test]
    fn renders_compact_detail_layout_when_height_is_limited() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text_with_size(detail, 100, 24);

        assert!(text.contains("Track"));
        assert!(text.contains("PnL"));
        assert!(text.contains("Execution Stats"));
        assert!(!text.contains("Trace"));
    }

    #[test]
    fn renders_minimal_detail_layout_when_height_is_tight() {
        let detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();

        let text = render_text_with_size(detail, 100, 16);

        assert!(text.contains("Track"));
        assert!(text.contains("PnL"));
        assert!(text.contains("Market"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("Execution"));
        assert!(!text.contains("Trace"));
        assert!(!text.contains("Execution Stats"));
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

        assert!(text.contains("Market"));
        assert!(text.contains("Strategy"));
        assert!(text.contains("PnL"));
        assert!(text.contains("Execution Stats"));
        assert!(text.contains("Execution"));
        assert!(text.contains("Trace"));
        assert!(text.contains("Activity"));
        assert!(!text.contains("Commands"));
        assert!(text.contains("commands: p pause"));
        assert!(text.contains("total ↑ +1245.30"));
        assert!(text.contains("realized cumulative ↑ +980.10"));
        assert!(text.contains("execution stats since: 2026-03-26T09:45:00Z"));
        assert!(text.contains("prices: ref 101.2500 | mark 101.3000 | index 101.2000"));
        assert!(text.contains("exposure: 3.5000 → 4.0000 [↑ +0.5000]"));
        assert!(text.contains("lower/upper: 90.0000 / 110.0000"));
        assert!(text.contains("units 8.0000/8.0000 | notional 375.0000"));
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
    fn renders_pnl_summary_with_explicit_separator_for_large_values() {
        let mut detail: TrackDetailView =
            serde_json::from_str(include_str!("../../tests/fixtures/track_detail_view.json"))
                .unwrap();
        detail.pnl.total_pnl = -123456789.12;
        detail.pnl.realized_pnl = 987654321.99;
        detail.pnl.unrealized_pnl = -111111111.11;

        let text = render_text(detail);

        assert!(text.contains("total ↓ -123456789.12"));
        assert!(text.contains("unrealized ↓ -111111111.11"));
        assert!(text.contains("realized cumulative ↑ +987654321.99"));
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

        assert!(text.contains("ATTENTION REQUIRED"));
        assert!(text.contains("recovery anomaly: unknown_live_order"));
        assert!(text.contains("gap 0.5000"));
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

        assert!(text.contains("active | open | ATTENTION REQUIRED | 3 reasons"));
        assert!(text.contains("gap 0.5000 | age 60000 ms"));
        assert!(text.contains("alerts: recovery anomaly: duplicate_live_orders | market data stale | insufficient account margin"));
    }
}
